/* HOLDFAST — Murmur dashboard client.
 *
 * Layers live updates onto the server-rendered shell: opens /ws for the live message/presence
 * stream and uses the JSON API for room switching, history, and sending. All message bodies are
 * inserted as DOM TEXT (never innerHTML), then bare http/https URLs are autolinked by building
 * <a> elements — so no user content is ever interpreted as HTML (XSS-safe), matching the
 * server-side escape-then-link rendering.
 */
(function () {
  "use strict";

  var cfg = window.MURMUR || {};
  var selected = cfg.selected || "lobby";
  var isMod = !!cfg.isMod; // whether the signed-in user may edit topics / pin messages
  var replyTarget = null; // id of the message the composer is currently replying to (or null)
  // The caller's own @mention handle (email local-part) — used to light the live mention dot.
  var myHandle = String(cfg.me || "").toLowerCase().split("@")[0];

  var $ = function (sel) { return document.querySelector(sel); };
  var timeline = $("#timeline");
  var roomList = $("#room-list");
  var roomTitle = $("#room-title");
  var roomTopic = $("#room-topic");
  var presence = $("#presence");
  var composer = $("#composer");
  var input = $("#composer-input");
  var pinnedPanel = $("#pinned-panel");
  var pinnedList = $("#pinned-list");
  var roomView = document.querySelector(".room-view");
  var sendBtn = composer ? composer.querySelector("button[type='submit']") : null;
  var sendLabel = sendBtn ? sendBtn.textContent : "Send";
  var jumpBtn = null; // the "jump to latest" affordance, lazily created

  // Honor the OS "reduce motion" setting for our few micro-interactions (smooth scroll).
  var reduceMotion = false;
  try {
    var mq = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = !!(mq && mq.matches);
    if (mq && mq.addEventListener) {
      mq.addEventListener("change", function (e) { reduceMotion = e.matches; });
    }
  } catch (e) { /* matchMedia unsupported — keep instant scrolling */ }

  // --- toast (transient write feedback via the Odyssey v2 .toast kit) --------
  var toastHost = null;
  function ensureToastHost() {
    if (toastHost) return toastHost;
    toastHost = document.createElement("div");
    toastHost.className = "toast-host";
    toastHost.setAttribute("aria-live", "polite");
    toastHost.setAttribute("aria-atomic", "true");
    document.body.appendChild(toastHost);
    return toastHost;
  }
  // Show a small toast. `kind` is "ok" (default) or "err". Text is inserted as textContent only.
  function toast(text, kind) {
    var host = ensureToastHost();
    var el = document.createElement("div");
    el.className = "toast" + (kind === "err" ? " toast--err" : " toast--ok");
    el.setAttribute("role", "status");
    el.textContent = String(text);
    host.appendChild(el);
    // Reflow-then-show so the fade-in transition runs; auto-dismiss after a short hold.
    requestAnimationFrame(function () { el.classList.add("is-in"); });
    setTimeout(function () {
      el.classList.remove("is-in");
      setTimeout(function () { if (el.parentNode) el.parentNode.removeChild(el); }, 200);
    }, 2600);
  }

  // --- helpers --------------------------------------------------------------

  function apiHeaders(json) {
    var h = { "X-CSRF-Token": cfg.csrf };
    if (json) h["Content-Type"] = "application/json";
    return h;
  }

  // Two-digit zero-padded.
  function pad(n) { return (n < 10 ? "0" : "") + n; }

  function fmtTime(epochSecs) {
    var d = new Date(epochSecs * 1000);
    return pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes());
  }

  // Build a message row as DOM (body inserted as safe text + autolinks).
  function buildMessage(m) {
    var row = document.createElement("div");
    row.className = "msg";
    row.setAttribute("data-id", m.id);
    var authorName = m.sender_email || m.sender_sub || "—";
    row.setAttribute("data-author", authorName);

    // Threaded reply: quote the parent (author + snippet) pulled from the on-screen parent row.
    if (m.reply_to_id) {
      var quote = buildQuote(m.reply_to_id);
      if (quote) row.appendChild(quote);
    }

    var head = document.createElement("div");
    head.className = "msg__head";
    var author = document.createElement("span");
    author.className = "msg__author";
    author.textContent = authorName;
    var time = document.createElement("span");
    time.className = "msg__time";
    time.textContent = fmtTime(m.created_at);
    head.appendChild(author);
    head.appendChild(time);
    if (m.edited_at && !m.deleted) {
      var edited = document.createElement("span");
      edited.className = "msg__edited";
      edited.textContent = "(edited)";
      head.appendChild(edited);
    }
    head.appendChild(buildTools());

    var body = document.createElement("div");
    body.className = "msg__body";
    if (m.deleted) {
      var del = document.createElement("span");
      del.className = "msg__deleted";
      del.textContent = "[deleted]";
      body.appendChild(del);
    } else {
      renderRichBody(body, m.body || "");
    }

    row.appendChild(head);
    row.appendChild(body);
    return row;
  }

  // The per-message "React" / "Reply" affordances (revealed on hover; wired by delegation).
  function buildTools() {
    var tools = document.createElement("span");
    tools.className = "msg__tools";
    tools.appendChild(makeTool("react", "React"));
    tools.appendChild(makeTool("reply", "Reply"));
    if (isMod) tools.appendChild(makeTool("pin", "Pin"));
    return tools;
  }
  function makeTool(act, label) {
    var b = document.createElement("button");
    b.type = "button";
    b.className = "msg__tool";
    b.setAttribute("data-act", act);
    b.textContent = label;
    return b;
  }

  // Build the quoted-parent block for a reply from the parent row already in the timeline.
  function buildQuote(parentId) {
    var parent = timeline
      ? timeline.querySelector('[data-id="' + cssEscape(parentId) + '"]')
      : null;
    if (!parent) return null;
    var pAuthor = parent.getAttribute("data-author") || "—";
    var pBodyEl = parent.querySelector(".msg__body");
    var pBody = pBodyEl ? pBodyEl.textContent.trim() : "";
    if (pBody.length > 120) pBody = pBody.slice(0, 120) + "…";
    var quote = document.createElement("div");
    quote.className = "msg__quote";
    quote.setAttribute("data-parent-id", parentId);
    var qa = document.createElement("span");
    qa.className = "msg__quote-author";
    qa.textContent = pAuthor;
    var qb = document.createElement("span");
    qb.className = "msg__quote-body";
    qb.textContent = pBody.replace(/[\r\n]+/g, " ");
    quote.appendChild(qa);
    quote.appendChild(qb);
    return quote;
  }

  // Build/replace a message's reaction chip row from a reactions array ([{emoji,count}]). Chips the
  // caller has reacted with (`mine`) are marked `is-mine` + aria-pressed for keyboard/AT clarity.
  function buildReactions(reactions, mine) {
    var mineSet = {};
    for (var mi = 0; mi < (mine || []).length; mi++) mineSet[mine[mi]] = true;
    var wrap = document.createElement("div");
    wrap.className = "msg__reactions";
    for (var i = 0; i < (reactions || []).length; i++) {
      var r = reactions[i];
      var chip = document.createElement("button");
      chip.type = "button";
      chip.className = "reaction" + (mineSet[r.emoji] ? " is-mine" : "");
      chip.setAttribute("data-emoji", r.emoji);
      chip.setAttribute("aria-pressed", mineSet[r.emoji] ? "true" : "false");
      chip.setAttribute("aria-label", "React " + r.emoji);
      var em = document.createElement("span");
      em.className = "reaction__emoji";
      em.textContent = r.emoji;
      var ct = document.createElement("span");
      ct.className = "reaction__count";
      ct.textContent = String(r.count);
      chip.appendChild(em);
      chip.appendChild(ct);
      wrap.appendChild(chip);
    }
    return wrap;
  }

  // Read a row's current reaction state back out of the DOM ({reactions:[{emoji,count}], mine:[]}).
  function readReactions(row) {
    var reactions = [];
    var chips = row.querySelectorAll(".reaction");
    for (var i = 0; i < chips.length; i++) {
      var emoji = chips[i].getAttribute("data-emoji");
      var cnt = parseInt((chips[i].querySelector(".reaction__count") || {}).textContent, 10) || 0;
      reactions.push({ emoji: emoji, count: cnt });
    }
    var mine = [];
    try { mine = JSON.parse(row.getAttribute("data-mine") || "[]"); } catch (e) { mine = []; }
    return { reactions: reactions, mine: mine };
  }

  // Replace (or drop) a message row's reaction chips in place. When `mine` is omitted the row's
  // existing "mine" set is preserved (live WS frames carry counts but not the viewer's own set).
  function applyReactions(msgId, reactions, mine) {
    if (!timeline) return;
    var row = timeline.querySelector('[data-id="' + cssEscape(msgId) + '"]');
    if (!row) return;
    if (mine === undefined) {
      try { mine = JSON.parse(row.getAttribute("data-mine") || "[]"); } catch (e) { mine = []; }
    }
    row.setAttribute("data-mine", JSON.stringify(mine || []));
    var existing = row.querySelector(".msg__reactions");
    if (!reactions || reactions.length === 0) {
      if (existing) existing.remove();
      return;
    }
    var next = buildReactions(reactions, mine);
    if (existing) existing.parentNode.replaceChild(next, existing);
    else row.appendChild(next);
  }

  // Compute the optimistic next reaction state for a local toggle of `emoji` (before the server
  // confirms): flip it in/out of `mine` and bump the matching chip's count, adding/removing chips.
  function optimisticToggle(state, emoji) {
    var mine = state.mine.slice();
    var reactions = state.reactions.map(function (r) { return { emoji: r.emoji, count: r.count }; });
    var had = mine.indexOf(emoji) !== -1;
    var idx = -1;
    for (var i = 0; i < reactions.length; i++) { if (reactions[i].emoji === emoji) { idx = i; break; } }
    if (had) {
      mine.splice(mine.indexOf(emoji), 1);
      if (idx !== -1) { reactions[idx].count -= 1; if (reactions[idx].count <= 0) reactions.splice(idx, 1); }
    } else {
      mine.push(emoji);
      if (idx !== -1) reactions[idx].count += 1;
      else reactions.push({ emoji: emoji, count: 1 });
    }
    return { reactions: reactions, mine: mine };
  }

  // Toggle a reaction OPTIMISTICALLY: update the chips immediately for instant feedback, then POST.
  // On success the server's authoritative counts + own-set replace the guess; on failure the prior
  // chips are restored and a toast explains why (progressive: the /react form route is unchanged).
  function toggleReaction(msgId, emoji) {
    if (!emoji) return;
    var row = timeline ? timeline.querySelector('[data-id="' + cssEscape(msgId) + '"]') : null;
    var snapshot = row ? readReactions(row) : null;
    if (snapshot) {
      var next = optimisticToggle(snapshot, emoji);
      applyReactions(msgId, next.reactions, next.mine);
    }
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages/" +
      encodeURIComponent(msgId) + "/react", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ emoji: emoji })
    })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(); })
      .then(function (data) { applyReactions(msgId, data.reactions, data.mine); })
      .catch(function () {
        if (snapshot) applyReactions(msgId, snapshot.reactions, snapshot.mine);
        toast("Couldn't save your reaction", "err");
      });
  }

  // Append text to `el`, turning bare http/https URLs into <a> elements. Uses text nodes so the
  // content can never be parsed as HTML.
  function appendLinkified(el, text) {
    var lines = String(text).split("\n");
    for (var li = 0; li < lines.length; li++) {
      if (li > 0) el.appendChild(document.createElement("br"));
      linkifyLine(el, lines[li]);
    }
  }

  function renderRichBody(el, text) {
    var lines = String(text).split("\n");
    var normal = [];
    var i = 0;
    while (i < lines.length) {
      var lang = fenceLang(lines[i]);
      if (lang !== null) {
        var close = findFenceClose(lines, i + 1);
        if (close !== -1) {
          flushNormal(el, normal);
          appendFence(el, lines.slice(i + 1, close), lang);
          i = close + 1;
          continue;
        }
      }
      normal.push(lines[i]);
      i++;
    }
    flushNormal(el, normal);
  }

  function flushNormal(el, lines) {
    for (var i = 0; i < lines.length; i++) {
      if (i > 0) el.appendChild(document.createElement("br"));
      renderNormalLine(el, lines[i]);
    }
    lines.length = 0;
  }

  function renderNormalLine(el, line) {
    var trimmed = String(line).replace(/^\s+/, "");
    if (trimmed.indexOf("> ") === 0) {
      var quote = document.createElement("blockquote");
      renderInline(quote, trimmed.slice(2));
      el.appendChild(quote);
    } else {
      renderInline(el, line);
    }
  }

  function renderInline(el, line) {
    line = String(line);
    var i = 0;
    var textStart = 0;
    while (i < line.length) {
      var ch = line.charAt(i);
      if (ch === "`") {
        var codeEnd = line.indexOf("`", i + 1);
        if (codeEnd > i + 1) {
          appendText(el, line.slice(textStart, i));
          appendWrappedText(el, "code", "", line.slice(i + 1, codeEnd));
          i = codeEnd + 1;
          textStart = i;
          continue;
        }
      }
      if (startsUrlAt(line, i)) {
        appendText(el, line.slice(textStart, i));
        var j = i;
        while (j < line.length && isUrlChar(line.charAt(j))) j++;
        var raw = line.slice(i, j);
        var trimmed = trimUrlTrailing(raw);
        appendUrlLink(el, trimmed);
        i = i + trimmed.length;
        textStart = i;
        continue;
      }
      if (ch === "@" && isMentionBoundary(line, i)) {
        var mentionTo = mentionEnd(line, i);
        if (mentionTo !== -1) {
          appendText(el, line.slice(textStart, i));
          var mention = document.createElement("span");
          mention.className = "mention";
          mention.textContent = "@" + line.slice(i + 1, mentionTo);
          el.appendChild(mention);
          i = mentionTo;
          textStart = i;
          continue;
        }
      }
      if ((ch === "*" || ch === "_" || ch === "~") && isFormatOpenBoundary(line, i)) {
        var fmtEnd = findClosingDelim(line, i + 1, ch);
        if (fmtEnd > i + 1) {
          appendText(el, line.slice(textStart, i));
          if (ch === "*") appendWrappedText(el, "strong", "", line.slice(i + 1, fmtEnd));
          else if (ch === "_") appendWrappedText(el, "em", "", line.slice(i + 1, fmtEnd));
          else appendWrappedText(el, "del", "", line.slice(i + 1, fmtEnd));
          i = fmtEnd + 1;
          textStart = i;
          continue;
        }
      }
      i += charLen(line, i);
    }
    appendText(el, line.slice(textStart));
  }

  function appendFence(el, lines, lang) {
    var pre = document.createElement("pre");
    var code = document.createElement("code");
    if (lang) code.className = "lang-" + lang;
    code.textContent = lines.join("\n");
    pre.appendChild(code);
    el.appendChild(pre);
  }

  function fenceLang(line) {
    var trimmed = String(line).trim();
    if (trimmed.slice(0, 3) !== "```") return null;
    var lang = trimmed.slice(3).trim();
    if (!lang) return "";
    var m = /\s/.exec(lang);
    return m ? lang.slice(0, m.index) : lang;
  }

  function isFenceClose(line) {
    return String(line).replace(/\s+$/, "") === "```";
  }

  function findFenceClose(lines, start) {
    for (var i = start; i < lines.length; i++) {
      if (isFenceClose(lines[i])) return i;
    }
    return -1;
  }

  var URL_RE = /(https?:\/\/[^\s]+)/g;
  function linkifyLine(el, line) {
    var last = 0, match;
    URL_RE.lastIndex = 0;
    while ((match = URL_RE.exec(line)) !== null) {
      if (match.index > last) {
        el.appendChild(document.createTextNode(line.slice(last, match.index)));
      }
      var raw = match[0];
      // Trim trailing sentence punctuation that is unlikely to be part of the URL.
      var trimmed = raw.replace(/[.,;:!?)\]]+$/, "");
      appendUrlLink(el, trimmed);
      if (trimmed.length < raw.length) {
        el.appendChild(document.createTextNode(raw.slice(trimmed.length)));
      }
      last = match.index + raw.length;
    }
    if (last < line.length) el.appendChild(document.createTextNode(line.slice(last)));
  }

  function startsUrlAt(line, i) {
    return line.slice(i, i + 7) === "http://" || line.slice(i, i + 8) === "https://";
  }

  function isUrlChar(ch) {
    return /^[A-Za-z0-9]$/.test(ch) ||
      ch === "-" || ch === "." || ch === "_" || ch === "~" || ch === ":" || ch === "/" ||
      ch === "?" || ch === "#" || ch === "[" || ch === "]" || ch === "@" || ch === "!" ||
      ch === "$" || ch === "&" || ch === "'" || ch === "(" || ch === ")" || ch === "*" ||
      ch === "+" || ch === "," || ch === ";" || ch === "=" || ch === "%";
  }

  function trimUrlTrailing(url) {
    return String(url).replace(/[.,;:!?)\]]+$/, "");
  }

  function appendUrlLink(el, url) {
    var a = document.createElement("a");
    a.setAttribute("href", url);
    a.rel = "noopener noreferrer nofollow";
    a.target = "_blank";
    a.textContent = url;
    el.appendChild(a);
  }

  function appendText(el, text) {
    if (text) el.appendChild(document.createTextNode(text));
  }

  function appendWrappedText(el, tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    node.textContent = text;
    el.appendChild(node);
  }

  function isMentionChar(ch) {
    return /^[A-Za-z0-9._-]$/.test(ch);
  }

  function isMentionBoundary(line, at) {
    if (at === 0) return true;
    var prev = line.charAt(at - 1);
    return !isMentionChar(prev) && prev !== "@";
  }

  function mentionEnd(line, at) {
    var j = at + 1;
    while (j < line.length && isMentionChar(line.charAt(j))) j++;
    return j > at + 1 ? j : -1;
  }

  function isFormatOpenBoundary(line, at) {
    if (at === 0) return true;
    return !isFormatWord(line.charAt(at - 1));
  }

  function isFormatCloseBoundary(line, after) {
    if (after >= line.length) return true;
    return !isFormatWord(line.charAt(after));
  }

  function isFormatWord(ch) {
    return /^[A-Za-z0-9_]$/.test(ch);
  }

  function findClosingDelim(line, start, delim) {
    var i = start;
    while (i < line.length) {
      var ch = line.charAt(i);
      if (ch === delim && (delim !== "_" || isFormatCloseBoundary(line, i + 1))) return i;
      i += charLen(line, i);
    }
    return -1;
  }

  function charLen(line, i) {
    var code = line.charCodeAt(i);
    return code >= 0xD800 && code <= 0xDBFF && i + 1 < line.length ? 2 : 1;
  }

  function scrollToBottom(smooth) {
    if (!timeline) return;
    if (smooth && !reduceMotion && timeline.scrollTo) {
      timeline.scrollTo({ top: timeline.scrollHeight, behavior: "smooth" });
    } else {
      timeline.scrollTop = timeline.scrollHeight;
    }
    hideJump();
  }

  function clearTimeline() { if (timeline) timeline.innerHTML = ""; }

  // --- jump-to-latest -------------------------------------------------------
  // How far from the bottom (px) counts as "scrolled up" — below this the timeline auto-follows.
  var NEAR_BOTTOM = 80;
  function nearBottom() {
    if (!timeline) return true;
    return timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < NEAR_BOTTOM;
  }
  function ensureJumpBtn() {
    if (jumpBtn || !roomView) return jumpBtn;
    jumpBtn = document.createElement("button");
    jumpBtn.type = "button";
    jumpBtn.className = "jump-latest";
    jumpBtn.hidden = true;
    jumpBtn.setAttribute("aria-label", "Jump to latest messages");
    var label = document.createElement("span");
    label.className = "jump-latest__label";
    label.textContent = "Jump to latest";
    jumpBtn.appendChild(label);
    jumpBtn.addEventListener("click", function () { scrollToBottom(true); });
    roomView.appendChild(jumpBtn);
    return jumpBtn;
  }
  function showJump(newCount) {
    var b = ensureJumpBtn();
    if (!b) return;
    b.hidden = false;
    var label = b.querySelector(".jump-latest__label");
    if (newCount > 0) {
      b.classList.add("has-new");
      if (label) label.textContent = newCount + (newCount === 1 ? " new message" : " new messages");
    }
  }
  function hideJump() {
    if (!jumpBtn) return;
    jumpBtn.hidden = true;
    jumpBtn.classList.remove("has-new");
    jumpBtn.__new = 0;
    var label = jumpBtn.querySelector(".jump-latest__label");
    if (label) label.textContent = "Jump to latest";
  }
  if (timeline) {
    timeline.addEventListener("scroll", function () {
      if (nearBottom()) hideJump();
      else if (jumpBtn && !jumpBtn.classList.contains("has-new")) showJump(0);
    });
  }

  // --- composer affordances (autosize + send-enabled + in-flight) ------------
  function autosize() {
    if (!input) return;
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 200) + "px";
  }
  function updateSendState() {
    if (!sendBtn) return;
    var empty = !input || input.value.trim().length === 0;
    sendBtn.disabled = empty || sendBtn.classList.contains("is-sending");
  }
  if (input) {
    input.addEventListener("input", function () { autosize(); updateSendState(); });
  }

  // --- room switching + history --------------------------------------------

  function selectRoom(id, name) {
    selected = id;
    if (roomTitle) roomTitle.textContent = name || id;
    if (input) input.placeholder = "Message " + (name || id) + "…";
    var items = roomList ? roomList.querySelectorAll(".room") : [];
    var topic = "";
    for (var i = 0; i < items.length; i++) {
      var match = items[i].getAttribute("data-room-id") === id;
      items[i].classList.toggle("is-active", match);
      if (match) topic = items[i].getAttribute("data-room-topic") || "";
    }
    if (roomTopic) roomTopic.textContent = topic;
    clearReply();
    clearUnread(id);       // opening a room marks it read — drop its badge
    loadPinned(id);        // refresh the pinned panel for the new room
    loadMessages(id);
  }

  function loadMessages(id) {
    fetch("/api/rooms/" + encodeURIComponent(id) + "/messages", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : { messages: [] }; })
      .then(function (data) {
        clearTimeline();
        var msgs = (data.messages || []).slice().reverse(); // server returns newest-first
        if (msgs.length === 0) {
          var empty = document.createElement("div");
          empty.className = "timeline__empty";
          empty.textContent = "No messages yet — say hello.";
          timeline.appendChild(empty);
        } else {
          for (var i = 0; i < msgs.length; i++) timeline.appendChild(buildMessage(msgs[i]));
        }
        scrollToBottom();
        markRead(id);
      })
      .catch(function () {});
  }

  function markRead(id) {
    fetch("/api/rooms/" + encodeURIComponent(id) + "/read", {
      method: "POST",
      headers: apiHeaders(false),
      credentials: "same-origin"
    }).catch(function () {});
  }

  // --- sending --------------------------------------------------------------

  if (composer) {
    composer.addEventListener("submit", function (e) {
      e.preventDefault();
      sendMessage();
    });
  }
  if (input) {
    input.addEventListener("keydown", function (e) {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        sendMessage();
      }
    });
  }

  function setSending(on) {
    if (!sendBtn) return;
    if (on) {
      sendBtn.classList.add("is-sending");
      sendBtn.disabled = true;
      sendBtn.textContent = "Sending…";
    } else {
      sendBtn.classList.remove("is-sending");
      sendBtn.textContent = sendLabel;
      updateSendState();
    }
  }

  function sendMessage() {
    var body = input ? input.value.trim() : "";
    if (!body) return;
    if (sendBtn && sendBtn.classList.contains("is-sending")) return; // guard double-submit
    var payload = { body: body };
    if (replyTarget) payload.reply_to_id = replyTarget;
    setSending(true);
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify(payload)
    })
      .then(function (r) {
        if (!r.ok) throw new Error("send failed");
        if (input) { input.value = ""; autosize(); }
        clearReply();
        // The message arrives back over /ws and is appended there (no optimistic dup).
      })
      .catch(function () { toast("Message not sent — check your connection", "err"); })
      .then(function () { setSending(false); });
  }

  // --- reactions + threaded-reply affordances (event delegation) -----------

  if (timeline) {
    timeline.addEventListener("click", function (e) {
      var chip = e.target.closest ? e.target.closest(".reaction") : null;
      if (chip) {
        var row = chip.closest(".msg");
        if (row) toggleReaction(row.getAttribute("data-id"), chip.getAttribute("data-emoji"));
        return;
      }
      var tool = e.target.closest ? e.target.closest(".msg__tool") : null;
      if (!tool) return;
      var msg = tool.closest(".msg");
      if (!msg) return;
      var id = msg.getAttribute("data-id");
      if (tool.getAttribute("data-act") === "react") {
        var emoji = window.prompt("React with (emoji)", "👍");
        if (emoji) toggleReaction(id, emoji.trim());
      } else if (tool.getAttribute("data-act") === "reply") {
        setReply(id, msg.getAttribute("data-author") || "");
      } else if (tool.getAttribute("data-act") === "pin") {
        pinMessage(id);
      }
    });
  }

  // Point the composer at a parent message and show a dismissible reply banner above it.
  function setReply(id, author) {
    replyTarget = id;
    var existing = document.getElementById("reply-banner");
    if (existing) existing.remove();
    var banner = document.createElement("div");
    banner.id = "reply-banner";
    banner.className = "reply-banner";
    var label = document.createElement("span");
    label.className = "reply-banner__label";
    label.textContent = "Replying to " + (author || "message");
    var cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "reply-banner__cancel";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", clearReply);
    banner.appendChild(label);
    banner.appendChild(cancel);
    if (composer && composer.parentNode) composer.parentNode.insertBefore(banner, composer);
    if (input) input.focus();
  }

  function clearReply() {
    replyTarget = null;
    var banner = document.getElementById("reply-banner");
    if (banner) banner.remove();
  }

  // --- room list clicks + new room -----------------------------------------

  if (roomList) {
    roomList.addEventListener("click", function (e) {
      var li = e.target.closest ? e.target.closest(".room") : null;
      if (!li) return;
      selectRoom(li.getAttribute("data-room-id"), li.getAttribute("data-room-name"));
    });
  }

  var newRoomBtn = document.getElementById("new-room");
  if (newRoomBtn) {
    newRoomBtn.addEventListener("click", function () {
      var name = window.prompt("New room name");
      if (!name) return;
      fetch("/api/rooms", {
        method: "POST",
        headers: apiHeaders(true),
        credentials: "same-origin",
        body: JSON.stringify({ name: name, kind: "room" })
      })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function (data) {
          if (!data || !data.room) return;
          addRoomToList(data.room);
          selectRoom(data.room.id, data.room.name);
        })
        .catch(function () {});
    });
  }

  function addRoomToList(room) {
    if (!roomList) return;
    if (roomList.querySelector('[data-room-id="' + cssEscape(room.id) + '"]')) return;
    var li = document.createElement("li");
    li.className = "room";
    li.setAttribute("data-room-id", room.id);
    li.setAttribute("data-room-name", room.name);
    li.setAttribute("data-room-topic", room.topic || "");
    var btn = document.createElement("button");
    btn.className = "room__btn";
    btn.type = "button";
    btn.textContent = room.name;
    li.appendChild(btn);
    // A badge scaffold (hidden until unread) so live bumps have somewhere to write.
    var badge = document.createElement("span");
    badge.className = "room__badge";
    var unread = document.createElement("span");
    unread.className = "room__unread";
    unread.hidden = true;
    unread.textContent = "0";
    badge.appendChild(unread);
    li.appendChild(badge);
    roomList.appendChild(li);
  }

  function cssEscape(s) { return String(s).replace(/["\\]/g, "\\$&"); }

  // --- new DM (people directory) -------------------------------------------

  var newDmBtn = document.getElementById("new-dm");
  if (newDmBtn) {
    newDmBtn.addEventListener("click", function () {
      fetch("/api/directory", { credentials: "same-origin" })
        .then(function (r) { return r.ok ? r.json() : { people: [] }; })
        .then(function (data) { openDirectoryPicker(data.people || []); })
        .catch(function () {});
    });
  }

  // Build a lightweight modal listing every person the user can DM; picking one opens the DM.
  function openDirectoryPicker(people) {
    closeDirectoryPicker();
    var overlay = document.createElement("div");
    overlay.className = "dm-picker";
    overlay.id = "dm-picker";
    var panel = document.createElement("div");
    panel.className = "dm-picker__panel";

    var head = document.createElement("div");
    head.className = "dm-picker__head";
    var title = document.createElement("h3");
    title.className = "dm-picker__title";
    title.textContent = "New direct message";
    var close = document.createElement("button");
    close.className = "btn btn-ghost btn-sm";
    close.type = "button";
    close.textContent = "Close";
    close.addEventListener("click", closeDirectoryPicker);
    head.appendChild(title);
    head.appendChild(close);
    panel.appendChild(head);

    var list = document.createElement("ul");
    list.className = "dm-picker__list";
    if (people.length === 0) {
      var empty = document.createElement("li");
      empty.className = "dm-picker__empty";
      empty.textContent = "No people to message yet.";
      list.appendChild(empty);
    } else {
      for (var i = 0; i < people.length; i++) {
        list.appendChild(buildPersonRow(people[i]));
      }
    }
    panel.appendChild(list);
    overlay.appendChild(panel);
    // Click on the backdrop (outside the panel) dismisses the picker.
    overlay.addEventListener("click", function (e) { if (e.target === overlay) closeDirectoryPicker(); });
    document.body.appendChild(overlay);
  }

  function buildPersonRow(person) {
    var li = document.createElement("li");
    var btn = document.createElement("button");
    btn.className = "dm-picker__person";
    btn.type = "button";
    btn.textContent = person.user_email || person.user_sub;
    btn.addEventListener("click", function () { openDm(person); });
    li.appendChild(btn);
    return li;
  }

  function closeDirectoryPicker() {
    var existing = document.getElementById("dm-picker");
    if (existing) existing.remove();
  }

  function openDm(person) {
    fetch("/api/dms", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ subject: person.user_sub, email: person.user_email })
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        closeDirectoryPicker();
        if (!data || !data.room) return;
        addRoomToList(data.room);
        selectRoom(data.room.id, data.room.name);
        // Re-open the live stream so the freshly-created DM room starts streaming immediately.
        reconnect();
      })
      .catch(function () {});
  }

  // --- room topic (mods) ----------------------------------------------------

  var topicEditBtn = document.getElementById("topic-edit");
  if (topicEditBtn && isMod) {
    topicEditBtn.hidden = false;
    topicEditBtn.addEventListener("click", editTopic);
  }

  function editTopic() {
    var current = roomTopic ? roomTopic.textContent : "";
    var next = window.prompt("Room topic", current);
    if (next === null) return; // cancelled
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/topic", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ topic: next })
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (!data || !data.room) return;
        var t = data.room.topic || "";
        if (roomTopic) roomTopic.textContent = t;
        // Keep the room-list data attribute in sync so re-selecting shows the new topic.
        var li = roomList
          ? roomList.querySelector('[data-room-id="' + cssEscape(selected) + '"]')
          : null;
        if (li) li.setAttribute("data-room-topic", t);
      })
      .catch(function () {});
  }

  // --- pinned messages ------------------------------------------------------

  var pinToggleBtn = document.getElementById("pin-toggle");
  if (pinToggleBtn) {
    pinToggleBtn.addEventListener("click", function () {
      if (!pinnedPanel) return;
      var show = pinnedPanel.hasAttribute("hidden");
      if (show) { pinnedPanel.removeAttribute("hidden"); loadPinned(selected); }
      else { pinnedPanel.setAttribute("hidden", ""); }
      pinToggleBtn.setAttribute("aria-expanded", show ? "true" : "false");
    });
  }

  function loadPinned(id) {
    if (!pinnedList) return;
    fetch("/api/rooms/" + encodeURIComponent(id) + "/pinned", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : { messages: [] }; })
      .then(function (data) { renderPinned(data.messages || []); })
      .catch(function () {});
  }

  function renderPinned(messages) {
    if (!pinnedList) return;
    pinnedList.innerHTML = "";
    if (messages.length === 0) {
      var empty = document.createElement("div");
      empty.className = "pinned-panel__empty";
      empty.textContent = "No pinned messages.";
      pinnedList.appendChild(empty);
      return;
    }
    for (var i = 0; i < messages.length; i++) {
      pinnedList.appendChild(buildPinnedItem(messages[i]));
    }
  }

  function buildPinnedItem(m) {
    var item = document.createElement("div");
    item.className = "pinned-item";
    item.setAttribute("data-jump-id", m.id);
    var jump = document.createElement("button");
    jump.type = "button";
    jump.className = "pinned-item__jump";
    var author = document.createElement("span");
    author.className = "pinned-item__author";
    author.textContent = m.sender_email || m.sender_sub || "—";
    var body = document.createElement("span");
    body.className = "pinned-item__body";
    var text = m.deleted ? "[deleted]" : String(m.body || "");
    text = text.replace(/[\r\n]+/g, " ");
    if (text.length > 140) text = text.slice(0, 140) + "…";
    body.textContent = text;
    jump.appendChild(author);
    jump.appendChild(body);
    item.appendChild(jump);
    if (isMod) {
      var unpin = document.createElement("button");
      unpin.type = "button";
      unpin.className = "pinned-item__unpin";
      unpin.setAttribute("data-unpin", m.id);
      unpin.textContent = "Unpin";
      item.appendChild(unpin);
    }
    return item;
  }

  if (pinnedList) {
    pinnedList.addEventListener("click", function (e) {
      var unpinBtn = e.target.closest ? e.target.closest("[data-unpin]") : null;
      if (unpinBtn) { unpinMessage(unpinBtn.getAttribute("data-unpin")); return; }
      var jumpEl = e.target.closest ? e.target.closest(".pinned-item") : null;
      if (jumpEl) jumpToMessage(jumpEl.getAttribute("data-jump-id"));
    });
  }

  function pinMessage(msgId) {
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages/" +
      encodeURIComponent(msgId) + "/pin", {
      method: "POST",
      headers: apiHeaders(false),
      credentials: "same-origin"
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (!data) return;
        renderPinned(data.messages || []);
        if (pinnedPanel) { pinnedPanel.removeAttribute("hidden"); }
        if (pinToggleBtn) pinToggleBtn.setAttribute("aria-expanded", "true");
      })
      .catch(function () {});
  }

  function unpinMessage(msgId) {
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages/" +
      encodeURIComponent(msgId) + "/unpin", {
      method: "POST",
      headers: apiHeaders(false),
      credentials: "same-origin"
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) { if (data) renderPinned(data.messages || []); })
      .catch(function () {});
  }

  function jumpToMessage(msgId) {
    if (!timeline) return;
    var row = timeline.querySelector('[data-id="' + cssEscape(msgId) + '"]');
    if (!row) return;
    row.scrollIntoView({ block: "center" });
    row.classList.add("msg--flash");
    setTimeout(function () { row.classList.remove("msg--flash"); }, 1200);
  }

  // --- search + mentions ----------------------------------------------------

  var searchForm = document.getElementById("search-form");
  var searchInput = document.getElementById("search-input");
  if (searchForm) {
    searchForm.addEventListener("submit", function (e) {
      e.preventDefault();
      var q = searchInput ? searchInput.value.trim() : "";
      if (q.length < 2) return;
      doSearch(q);
    });
  }

  var mentionsBtn = document.getElementById("mentions-btn");
  if (mentionsBtn) {
    mentionsBtn.addEventListener("click", loadMentions);
  }

  function doSearch(q) {
    fetch("/api/search?q=" + encodeURIComponent(q), { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : { results: [] }; })
      .then(function (data) { openResults("Search: " + q, data.results || []); })
      .catch(function () {});
  }

  function loadMentions() {
    fetch("/api/mentions", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : { results: [] }; })
      .then(function (data) { openResults("Your mentions", data.results || []); })
      .catch(function () {});
  }

  // A lightweight results overlay (reuses the DM-picker chrome). Each hit links back to its room.
  function openResults(title, results) {
    closeResults();
    var overlay = document.createElement("div");
    overlay.className = "dm-picker";
    overlay.id = "results-picker";
    var panel = document.createElement("div");
    panel.className = "dm-picker__panel";

    var head = document.createElement("div");
    head.className = "dm-picker__head";
    var h = document.createElement("h3");
    h.className = "dm-picker__title";
    h.textContent = title;
    var close = document.createElement("button");
    close.className = "btn btn-ghost btn-sm";
    close.type = "button";
    close.textContent = "Close";
    close.addEventListener("click", closeResults);
    head.appendChild(h);
    head.appendChild(close);
    panel.appendChild(head);

    var list = document.createElement("ul");
    list.className = "dm-picker__list";
    if (results.length === 0) {
      var empty = document.createElement("li");
      empty.className = "dm-picker__empty";
      empty.textContent = "No matches.";
      list.appendChild(empty);
    } else {
      for (var i = 0; i < results.length; i++) {
        list.appendChild(buildResultRow(results[i]));
      }
    }
    panel.appendChild(list);
    overlay.appendChild(panel);
    overlay.addEventListener("click", function (e) { if (e.target === overlay) closeResults(); });
    document.body.appendChild(overlay);
  }

  function buildResultRow(hit) {
    var li = document.createElement("li");
    var btn = document.createElement("button");
    btn.className = "result-row";
    btn.type = "button";
    var room = document.createElement("span");
    room.className = "result-row__room";
    room.textContent = hit.room_name || hit.room_id || "—";
    var meta = document.createElement("span");
    meta.className = "result-row__meta";
    meta.textContent = (hit.sender_email || hit.sender_sub || "—") + " · " + fmtTime(hit.created_at);
    var body = document.createElement("span");
    body.className = "result-row__body";
    body.textContent = String(hit.body || "");
    btn.appendChild(room);
    btn.appendChild(meta);
    btn.appendChild(body);
    btn.addEventListener("click", function () {
      closeResults();
      openRoomFromResult(hit);
    });
    li.appendChild(btn);
    return li;
  }

  // Select the hit's room (adding it to the sidebar if it is not already listed), then jump to the
  // matched message once its history loads.
  function openRoomFromResult(hit) {
    var li = roomList
      ? roomList.querySelector('[data-room-id="' + cssEscape(hit.room_id) + '"]')
      : null;
    var name = li ? li.getAttribute("data-room-name") : (hit.room_name || hit.room_id);
    if (!li) addRoomToList({ id: hit.room_id, name: hit.room_name || hit.room_id, topic: "" });
    selectRoom(hit.room_id, name);
    // Give the timeline a moment to load, then flash the target message if present.
    setTimeout(function () { jumpToMessage(hit.id); }, 250);
  }

  function closeResults() {
    var existing = document.getElementById("results-picker");
    if (existing) existing.remove();
  }

  // --- unread badges --------------------------------------------------------

  // Increment a background room's unread count (and optionally light its mention dot).
  function bumpUnread(roomId, mention) {
    if (!roomList) return;
    var li = roomList.querySelector('[data-room-id="' + cssEscape(roomId) + '"]');
    if (!li) return;
    var count = li.querySelector(".room__unread");
    if (count) {
      var n = parseInt(count.textContent, 10) || 0;
      count.textContent = String(n + 1);
      count.hidden = false;
    }
    if (mention) ensureMentionDot(li);
  }

  // Clear a room's unread badge + mention dot (called when the room is opened / read).
  function clearUnread(roomId) {
    if (!roomList) return;
    var li = roomList.querySelector('[data-room-id="' + cssEscape(roomId) + '"]');
    if (!li) return;
    var count = li.querySelector(".room__unread");
    if (count) { count.textContent = "0"; count.hidden = true; }
    var dot = li.querySelector(".room__mention");
    if (dot) dot.remove();
  }

  function ensureMentionDot(li) {
    var badge = li.querySelector(".room__badge");
    if (!badge || badge.querySelector(".room__mention")) return;
    var dot = document.createElement("span");
    dot.className = "room__mention";
    dot.title = "You were mentioned";
    badge.insertBefore(dot, badge.firstChild);
  }

  // Does a message body @mention the signed-in user (by email local-part)?
  function mentionsMe(body) {
    if (!myHandle) return false;
    var re = /(^|[^A-Za-z0-9._@-])@([A-Za-z0-9._-]+)/g, m;
    while ((m = re.exec(body)) !== null) {
      if (m[2].toLowerCase() === myHandle) return true;
    }
    return false;
  }

  // --- live stream (WebSocket) ---------------------------------------------

  var ws = null, retry = 0;
  function connect() {
    var proto = location.protocol === "https:" ? "wss" : "ws";
    try {
      ws = new WebSocket(proto + "://" + location.host + "/ws");
    } catch (e) { scheduleReconnect(); return; }

    ws.onopen = function () { retry = 0; if (presence) presence.textContent = "live"; };
    ws.onmessage = function (ev) {
      var frame;
      try { frame = JSON.parse(ev.data); } catch (e) { return; }
      if (frame.type === "message") onLiveMessage(frame);
      else if (frame.type === "presence") onPresence(frame);
      else if (frame.type === "reaction") onReaction(frame);
    };
    ws.onclose = function () { if (presence) presence.textContent = ""; scheduleReconnect(); };
    ws.onerror = function () { if (ws) ws.close(); };
  }

  function scheduleReconnect() {
    retry = Math.min(retry + 1, 6);
    setTimeout(connect, 500 * Math.pow(2, retry));
  }

  // Force an immediate reconnect (e.g. after joining/creating a room) so the socket re-subscribes
  // to the caller's current room set — the server snapshots membership at connect time.
  function reconnect() {
    retry = 0;
    if (ws) { try { ws.onclose = null; ws.close(); } catch (e) {} ws = null; }
    connect();
  }

  function onLiveMessage(frame) {
    if (frame.room_id !== selected || !timeline) {
      // A message in a room the user is NOT looking at: bump that room's unread badge (never for
      // the user's own posts) and light the mention dot when the body @mentions them.
      if (frame.room_id !== selected && !frame.deleted && frame.sender_email !== cfg.me) {
        bumpUnread(frame.room_id, mentionsMe(frame.body || ""));
      }
      return;
    }
    // An edit/soft-delete arrives with the SAME id as an on-screen row: update it in place.
    var existing = timeline.querySelector('[data-id="' + cssEscape(frame.id) + '"]');
    if (existing) {
      existing.parentNode.replaceChild(buildMessage(frame), existing);
      return;
    }
    var emptyEl = timeline.querySelector(".timeline__empty");
    if (emptyEl) emptyEl.remove();
    var wasNear = nearBottom();
    var mine = frame.sender_email === cfg.me;
    timeline.appendChild(buildMessage(frame));
    if (wasNear || mine) {
      scrollToBottom(true);
    } else {
      // Reader is scrolled up: don't yank them — surface a "N new messages" jump affordance.
      ensureJumpBtn();
      var n = (jumpBtn.__new || 0) + 1;
      jumpBtn.__new = n;
      showJump(n);
    }
    markRead(selected);
  }

  function onReaction(frame) {
    if (frame.room_id !== selected) return;
    applyReactions(frame.message_id, frame.reactions);
  }

  function onPresence(frame) {
    if (frame.room_id !== selected || !presence) return;
    presence.textContent = frame.user_email + " " + frame.status;
    if (frame.status === "online") {
      setTimeout(function () { if (presence.textContent.indexOf(frame.user_email) === 0) presence.textContent = "live"; }, 3000);
    }
  }

  // --- boot -----------------------------------------------------------------

  scrollToBottom();
  autosize();
  updateSendState();
  if (selected) markRead(selected);
  connect();
})();
