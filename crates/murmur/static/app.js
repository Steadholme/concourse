/* Murmur — The Talkback Patchbay client.
 *
 * Layers live behaviour onto the server-rendered Patchbay shell. Truths it keeps:
 *  - the composer clears only after a parsed 201 + persisted receipt; there is no server
 *    idempotency key, so the UI never claims retry is safe, and never says "Delivered";
 *  - every room switch / locator bumps a browser-local SelectionEpoch; late room-scoped
 *    responses can no longer mutate the DOM;
 *  - the transport meter reports only Unknown / Connected / Reconnecting / Offline, with
 *    "Catching up" as a separate reconcile flag (there is no Revoked frame in v1);
 *  - unread/mention marks clear only after a persisted read-marker response;
 *  - message bodies are inserted as DOM text and linkified by building <a> elements —
 *    user content is never parsed as HTML (mirrors the server renderer in text.rs).
 */
(function () {
  "use strict";

  document.documentElement.classList.add("js");

  // --- boot -----------------------------------------------------------------

  function readBoot() {
    var el = document.getElementById("pb-boot");
    if (!el) return null;
    try { return JSON.parse(el.textContent); } catch (e) { return null; }
  }
  var boot = readBoot() || {};
  var csrf = typeof boot.csrf === "string" ? boot.csrf : "";
  var me = boot.me && typeof boot.me.display_email === "string" ? boot.me.display_email : "";
  var myHandle = me.toLowerCase().split("@")[0];
  var isMod = !!boot.isMod;
  var limits = boot.limits || {};
  var BODY_MAX = limits.body || 8192;
  var SEARCH_MIN = limits.searchMin || 2;
  var selected = boot.selected && boot.selected.room_id ? String(boot.selected.room_id) : "";
  var selectedKind = boot.selected && boot.selected.kind ? String(boot.selected.kind) : "room";
  var selectedAuthority =
    boot.selected && boot.selected.authority === "archived_read_only" ? "archived" : "active";

  var $ = function (sel) { return document.querySelector(sel); };
  var deck = $("#pb-deck");
  var tape = $("#pb-tape");
  var tapeStatus = $("#pb-tape-status");
  var jackList = $("#pb-jack-list");
  var jackfield = $("#pb-jackfield");
  var roomsOpenBtn = $("#pb-rooms-open");
  var patchbarCurrent = $("#pb-patchbar-current");
  var roomTitle = $("#pb-room-title");
  var roomTopic = $("#pb-room-topic");
  var cue = $("#pb-cue");
  var cueInput = $("#pb-cue-input");
  var cueSend = $("#pb-cue-send");
  var cueBudget = $("#pb-cue-budget");
  var cueRoom = $("#pb-cue-room");
  var receipt = $("#pb-receipt");
  var loopLine = $("#pb-cue-loop");
  var loopText = $("#pb-cue-loop-text");
  var loopCancel = $("#pb-cue-loop-cancel");
  var replyField = $("#pb-reply-to");
  var transport = $("#pb-transport");
  var transportLabel = $("#pb-transport-label");
  var catchupTag = $("#pb-catchup");
  var reconnectBtn = $("#pb-reconnect");
  var ledger = $("#pb-ledger");
  var ledgerToggle = $("#pb-ledger-toggle");
  var ledgerClose = $("#pb-ledger-close");
  var ledgerResults = $("#pb-ledger-results");
  var ledgerSearch = $("#pb-ledger-search");
  var ledgerQ = $("#pb-ledger-q");
  var ledgerRoom = $("#pb-ledger-room");
  var livePolite = $("#pb-live-polite");
  var presenceHost = $("#pb-presence");
  var newRoomBtn = $("#pb-new-room");
  var newDmBtn = $("#pb-new-dm");

  if (!deck || !tape) return; // nothing to enhance on fallback pages

  // Browser-local selection epoch: every room switch / locator attempt increments it, and any
  // room-scoped response captured under an older epoch may not touch the DOM.
  var selectionEpoch = 0;
  function currentEpoch() { return selectionEpoch; }
  function stale(ep) { return ep !== selectionEpoch; }

  var reduceMotion = false;
  try {
    var mq = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)");
    reduceMotion = !!(mq && mq.matches);
    if (mq && mq.addEventListener) {
      mq.addEventListener("change", function (e) { reduceMotion = e.matches; });
    }
  } catch (e) { /* keep instant behaviour */ }

  var narrowMq = window.matchMedia ? window.matchMedia("(max-width: 767px)") : null;
  var ledgerRailMq = window.matchMedia ? window.matchMedia("(min-width: 1024px)") : null;

  // --- small helpers ----------------------------------------------------------

  function apiHeaders(json) {
    var h = { "X-CSRF-Token": csrf };
    if (json) h["Content-Type"] = "application/json";
    return h;
  }
  function pad(n) { return (n < 10 ? "0" : "") + n; }
  function fmtTime(epochSecs) {
    var d = new Date(epochSecs * 1000);
    return pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes());
  }
  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }
  function cssEscape(s) { return String(s).replace(/["\\]/g, "\\$&"); }
  function rowFor(msgId) {
    return tape.querySelector('[data-msg-id="' + cssEscape(msgId) + '"]');
  }

  // Error wire contract → safe copy. Raw backend detail never enters the DOM; only the
  // 400 field-level validation message is shown verbatim because the server vets it.
  var ERROR_COPY = {
    unauthorized: "Session required",
    csrf_invalid: "Request could not be verified",
    forbidden: "Action not allowed",
    not_found: "Resource unavailable",
    room_archived: "Room is archived and read-only",
    conflict: "State changed; refresh and retry",
    unavailable: "Murmur is temporarily unavailable"
  };
  function safeErrorCopy(payload, fallbackStatus) {
    if (payload && typeof payload.error === "string") {
      if (payload.error === "invalid_request" && typeof payload.message === "string") {
        return payload.message;
      }
      if (ERROR_COPY[payload.error]) return ERROR_COPY[payload.error];
    }
    if (fallbackStatus === 401) return ERROR_COPY.unauthorized;
    if (fallbackStatus === 403) return ERROR_COPY.forbidden;
    if (fallbackStatus === 404) return ERROR_COPY.not_found;
    if (fallbackStatus === 409) return ERROR_COPY.conflict;
    if (fallbackStatus >= 500) return ERROR_COPY.unavailable;
    return "Action not allowed";
  }
  function parseErrorResponse(r) {
    return r
      .json()
      .catch(function () { return null; })
      .then(function (payload) { return safeErrorCopy(payload, r.status); });
  }

  // --- rich body renderer (mirrors crates/murmur/src/text.rs render_body) ----

  function renderRichBody(host, text) {
    var lines = String(text).split("\n");
    var normal = [];
    var i = 0;
    while (i < lines.length) {
      var lang = fenceLang(lines[i]);
      if (lang !== null) {
        var close = findFenceClose(lines, i + 1);
        if (close !== -1) {
          flushNormal(host, normal);
          appendFence(host, lines.slice(i + 1, close), lang);
          i = close + 1;
          continue;
        }
      }
      normal.push(lines[i]);
      i++;
    }
    flushNormal(host, normal);
  }
  function flushNormal(host, lines) {
    for (var i = 0; i < lines.length; i++) {
      if (i > 0) host.appendChild(document.createElement("br"));
      renderNormalLine(host, lines[i]);
    }
    lines.length = 0;
  }
  function renderNormalLine(host, line) {
    var trimmed = String(line).replace(/^\s+/, "");
    if (trimmed.indexOf("> ") === 0) {
      var quote = el("blockquote");
      renderInline(quote, trimmed.slice(2));
      host.appendChild(quote);
    } else {
      renderInline(host, line);
    }
  }
  function renderInline(host, line) {
    line = String(line);
    var i = 0;
    var textStart = 0;
    while (i < line.length) {
      var ch = line.charAt(i);
      if (ch === "`") {
        var codeEnd = line.indexOf("`", i + 1);
        if (codeEnd > i + 1) {
          appendText(host, line.slice(textStart, i));
          appendWrapped(host, "code", "", line.slice(i + 1, codeEnd));
          i = codeEnd + 1;
          textStart = i;
          continue;
        }
      }
      if (startsUrlAt(line, i)) {
        appendText(host, line.slice(textStart, i));
        var j = i;
        while (j < line.length && isUrlChar(line.charAt(j))) j++;
        var trimmed = trimUrlTrailing(line.slice(i, j));
        appendUrlLink(host, trimmed);
        i += trimmed.length;
        textStart = i;
        continue;
      }
      if (ch === "@" && isMentionBoundary(line, i)) {
        var mentionTo = mentionEnd(line, i);
        if (mentionTo !== -1) {
          appendText(host, line.slice(textStart, i));
          var mention = el("span", "mention", "@" + line.slice(i + 1, mentionTo));
          host.appendChild(mention);
          i = mentionTo;
          textStart = i;
          continue;
        }
      }
      if ((ch === "*" || ch === "_" || ch === "~") && isFormatOpenBoundary(line, i)) {
        var fmtEnd = findClosingDelim(line, i + 1, ch);
        if (fmtEnd > i + 1) {
          appendText(host, line.slice(textStart, i));
          if (ch === "*") appendWrapped(host, "strong", "", line.slice(i + 1, fmtEnd));
          else if (ch === "_") appendWrapped(host, "em", "", line.slice(i + 1, fmtEnd));
          else appendWrapped(host, "del", "", line.slice(i + 1, fmtEnd));
          i = fmtEnd + 1;
          textStart = i;
          continue;
        }
      }
      i += charLen(line, i);
    }
    appendText(host, line.slice(textStart));
  }
  function appendFence(host, lines, lang) {
    var pre = el("pre");
    var code = el("code");
    if (lang) code.className = "lang-" + lang;
    code.textContent = lines.join("\n");
    pre.appendChild(code);
    host.appendChild(pre);
  }
  function fenceLang(line) {
    var trimmed = String(line).trim();
    if (trimmed.slice(0, 3) !== "```") return null;
    var lang = trimmed.slice(3).trim();
    if (!lang) return "";
    var m = /\s/.exec(lang);
    return m ? lang.slice(0, m.index) : lang;
  }
  function isFenceClose(line) { return String(line).replace(/\s+$/, "") === "```"; }
  function findFenceClose(lines, start) {
    for (var i = start; i < lines.length; i++) if (isFenceClose(lines[i])) return i;
    return -1;
  }
  function startsUrlAt(line, i) {
    return line.slice(i, i + 7) === "http://" || line.slice(i, i + 8) === "https://";
  }
  function isUrlChar(ch) {
    return /^[A-Za-z0-9]$/.test(ch) ||
      "-._~:/?#[]@!$&'()*+,;=%".indexOf(ch) !== -1;
  }
  function trimUrlTrailing(url) { return String(url).replace(/[.,;:!?)\]]+$/, ""); }
  function appendUrlLink(host, url) {
    var a = el("a", "", url);
    a.setAttribute("href", url);
    a.rel = "noopener noreferrer nofollow";
    a.target = "_blank";
    host.appendChild(a);
  }
  function appendText(host, text) { if (text) host.appendChild(document.createTextNode(text)); }
  function appendWrapped(host, tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    node.textContent = text;
    host.appendChild(node);
  }
  function isMentionChar(ch) { return /^[A-Za-z0-9._-]$/.test(ch); }
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
  function isFormatWord(ch) { return /^[A-Za-z0-9_]$/.test(ch); }
  function isFormatOpenBoundary(line, at) {
    return at === 0 || !isFormatWord(line.charAt(at - 1));
  }
  function isFormatCloseBoundary(line, after) {
    return after >= line.length || !isFormatWord(line.charAt(after));
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
  function mentionsMe(body) {
    if (!myHandle) return false;
    var re = /(^|[^A-Za-z0-9._@-])@([A-Za-z0-9._-]+)/g, m;
    while ((m = re.exec(body)) !== null) {
      if (m[2].toLowerCase() === myHandle) return true;
    }
    return false;
  }

  // --- message row construction (frozen DOM) ----------------------------------
  // <li class="msg" id="msg-{id}" data-msg-id data-created-at data-lifecycle data-own>

  function lifecycleOf(m) {
    if (!m.deleted) return "live";
    return m.body && m.body.length > 0 ? "redacted" : "deleted";
  }

  function buildRow(m) {
    var li = el("li", "msg");
    li.id = "msg-" + m.id;
    li.setAttribute("data-msg-id", m.id);
    li.setAttribute("data-created-at", String(m.created_at || 0));
    li.setAttribute("data-lifecycle", lifecycleOf(m));
    var own = !!me && m.sender_email === me;
    li.setAttribute("data-own", own ? "true" : "false");

    if (m.reply_to_id) {
      var loop = buildLoop(m.reply_to_id);
      if (loop) li.appendChild(loop);
    }

    var head = el("div", "msg__head");
    head.appendChild(el("span", "msg__author", m.sender_email || m.sender_sub || "—"));
    var time = el("time", "msg__time", fmtTime(m.created_at || 0));
    time.setAttribute("datetime", new Date((m.created_at || 0) * 1000).toISOString());
    head.appendChild(time);
    if (m.edited_at && !m.deleted) head.appendChild(el("span", "msg__edited", "(edited)"));
    head.appendChild(buildTools(li, m, own));

    var body = el("div", "msg__body");
    if (m.deleted) {
      body.appendChild(el("span", "msg__deleted",
        m.body && m.body.length > 0 ? m.body : "[deleted]"));
    } else {
      renderRichBody(body, m.body || "");
    }

    li.appendChild(head);
    li.appendChild(body);
    // Forward-compat: if the messages payload ever carries tallies (SSR parity), render them.
    if (m.reactions && m.reactions.length) {
      li.appendChild(buildReactions(m.reactions, m.mine || mineSet(m.id)));
    }
    return li;
  }

  // The per-message action cluster. Reply/React/Pin come from the SSR row too; Edit/Delete
  // are added here for own messages. Everything is a labelled button — no hover-only reveal.
  // Archived rooms are read-only: the cluster stays empty for SSR and JS rows alike, so no
  // write control is ever built unless the selected room authority is active.
  function buildTools(row, m, own) {
    var tools = el("span", "msg__tools");
    if (selectedAuthority !== "active") return tools;
    tools.appendChild(makeTool("reply", "Reply"));
    tools.appendChild(makeTool("react", "React"));
    if (isMod) tools.appendChild(makeTool("pin", "Pin"));
    if (own && !m.deleted) {
      tools.appendChild(makeTool("edit", "Edit"));
      tools.appendChild(makeTool("delete", "Delete"));
    }
    return tools;
  }
  function makeTool(act, label) {
    var b = el("button", "msg__tool", label);
    b.type = "button";
    b.setAttribute("data-act", act);
    return b;
  }

  // Reply loop: copper notch + author + ≤120-char snippet when the parent is inside the
  // loaded window; otherwise an honest "not in the latest window" locate link. Never an
  // existence leak: a deleted parent shows its tombstone label only.
  function buildLoop(parentId) {
    var loop = el("div", "loop");
    var parent = rowFor(parentId);
    if (!parent) {
      var off = el("a", "loop__link", "Loop — target not in the latest window · Locate");
      off.setAttribute("href", "/?room=" + encodeURIComponent(selected) +
        "&message=" + encodeURIComponent(parentId) + "#msg-" + encodeURIComponent(parentId));
      off.setAttribute("data-locate", parentId);
      loop.appendChild(off);
      return loop;
    }
    var link = el("a", "loop__link");
    link.setAttribute("href", "#msg-" + parentId);
    link.setAttribute("data-locate", parentId);
    var lifecycle = parent.getAttribute("data-lifecycle");
    if (lifecycle === "deleted" || lifecycle === "redacted") {
      link.appendChild(el("span", "loop__author", lifecycle === "redacted" ? "Redacted" : "Deleted"));
    } else {
      var authorEl = parent.querySelector(".msg__author");
      var bodyEl = parent.querySelector(".msg__body");
      var snippet = bodyEl ? bodyEl.textContent.replace(/[\r\n]+/g, " ").trim() : "";
      if (snippet.length > 120) snippet = snippet.slice(0, 120) + "…";
      link.appendChild(el("span", "loop__author",
        authorEl ? authorEl.textContent : "—"));
      link.appendChild(el("span", "loop__snippet", snippet));
    }
    loop.appendChild(link);
    return loop;
  }

  // --- reactions ----------------------------------------------------------------

  // Local "mine" sets per message (memory only — the server remains the authority and every
  // toggle is reconciled from its response).
  var mineByMsg = {};
  function mineSet(msgId) {
    if (!mineByMsg[msgId]) mineByMsg[msgId] = [];
    return mineByMsg[msgId];
  }

  function buildReactions(reactions, mine) {
    var mineLookup = {};
    (mine || []).forEach(function (e) { mineLookup[e] = true; });
    var wrap = el("div", "msg__reactions");
    (reactions || []).forEach(function (r) {
      var isMine = !!mineLookup[r.emoji];
      var chip = el("button", "reaction" + (isMine ? " is-mine" : ""));
      chip.type = "button";
      chip.setAttribute("data-emoji", r.emoji);
      chip.setAttribute("aria-pressed", isMine ? "true" : "false");
      chip.setAttribute("aria-label", "React " + r.emoji);
      chip.appendChild(el("span", "reaction__emoji", r.emoji));
      chip.appendChild(el("span", "reaction__count", String(r.count)));
      wrap.appendChild(chip);
    });
    return wrap;
  }

  function readChips(row) {
    var reactions = [];
    var chips = row.querySelectorAll(".reaction");
    for (var i = 0; i < chips.length; i++) {
      var cnt = chips[i].querySelector(".reaction__count");
      reactions.push({
        emoji: chips[i].getAttribute("data-emoji"),
        count: parseInt(cnt ? cnt.textContent : "0", 10) || 0
      });
    }
    return reactions;
  }

  function applyReactions(msgId, reactions, mine) {
    var row = rowFor(msgId);
    if (!row) return;
    if (mine !== undefined) mineByMsg[msgId] = mine.slice();
    else mine = mineSet(msgId);
    var existing = row.querySelector(".msg__reactions");
    if (!reactions || reactions.length === 0) {
      if (existing) existing.remove();
      return;
    }
    var next = buildReactions(reactions, mine);
    if (existing) existing.parentNode.replaceChild(next, existing);
    else row.appendChild(next);
  }

  function toggleReaction(msgId, emoji) {
    if (!emoji) return;
    var row = rowFor(msgId);
    if (!row) return;
    var requestRoom = selected;
    var ep = currentEpoch();
    var snapshot = { reactions: readChips(row), mine: mineSet(msgId).slice() };
    // Optimistic flip for instant feedback; the server response replaces it wholesale.
    var mine = snapshot.mine.slice();
    var reactions = snapshot.reactions.map(function (r) { return { emoji: r.emoji, count: r.count }; });
    var had = mine.indexOf(emoji) !== -1;
    var idx = -1;
    for (var i = 0; i < reactions.length; i++) if (reactions[i].emoji === emoji) idx = i;
    if (had) {
      mine.splice(mine.indexOf(emoji), 1);
      if (idx !== -1) { reactions[idx].count -= 1; if (reactions[idx].count <= 0) reactions.splice(idx, 1); }
    } else {
      mine.push(emoji);
      if (idx !== -1) reactions[idx].count += 1;
      else reactions.push({ emoji: emoji, count: 1 });
    }
    applyReactions(msgId, reactions, mine);
    fetch("/api/rooms/" + encodeURIComponent(requestRoom) + "/messages/" +
      encodeURIComponent(msgId) + "/react", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ emoji: emoji })
    })
      .then(function (r) {
        if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
        return r.json();
      })
      .then(function (data) {
        if (stale(ep) || selected !== requestRoom) return;
        applyReactions(msgId, data.reactions, data.mine);
      })
      .catch(function (err) {
        if (stale(ep) || selected !== requestRoom) return;
        applyReactions(msgId, snapshot.reactions, snapshot.mine);
        rowNote(row, "Couldn’t save your reaction — " + (err && err.message ? err.message : "try again"));
      });
  }

  // A transient, text-first inline note inside a message row (never a toast-only error).
  function rowNote(row, text) {
    var old = row.querySelector(".msg__note");
    if (old) old.remove();
    var note = el("p", "msg__note", text);
    note.setAttribute("role", "alert");
    row.appendChild(note);
    setTimeout(function () { if (note.parentNode) note.parentNode.removeChild(note); }, 5000);
  }

  // The React emoji popover: eight common patches plus a free input — no prompt().
  var EMOJI_SET = ["👍", "❤️", "😂", "🎉", "👀", "🙌", "✅", "🚀"];
  var openPopover = null;
  function closePopover() {
    if (openPopover && openPopover.parentNode) openPopover.parentNode.removeChild(openPopover);
    openPopover = null;
  }
  function openReactPopover(toolBtn, msgId) {
    closePopover();
    var pop = el("span", "msg__reactpop");
    pop.setAttribute("role", "group");
    pop.setAttribute("aria-label", "Pick a reaction");
    EMOJI_SET.forEach(function (emoji) {
      var b = el("button", "msg__reactpop-emoji", emoji);
      b.type = "button";
      b.setAttribute("aria-label", "React " + emoji);
      b.addEventListener("click", function () {
        closePopover();
        toggleReaction(msgId, emoji);
      });
      pop.appendChild(b);
    });
    var input = el("input", "msg__reactpop-input");
    input.type = "text";
    input.maxLength = 32;
    input.setAttribute("aria-label", "Custom emoji");
    var add = el("button", "msg__reactpop-add", "Add");
    add.type = "button";
    function submitCustom() {
      var value = input.value.trim();
      if (!value) return;
      closePopover();
      toggleReaction(msgId, value);
    }
    add.addEventListener("click", submitCustom);
    input.addEventListener("keydown", function (e) {
      if (e.key === "Enter") { e.preventDefault(); submitCustom(); }
      e.stopPropagation();
    });
    pop.appendChild(input);
    pop.appendChild(add);
    toolBtn.parentNode.appendChild(pop);
    openPopover = pop;
    input.focus();
  }
  document.addEventListener("click", function (e) {
    if (openPopover && !(e.target.closest && e.target.closest(".msg__reactpop")) &&
        !(e.target.closest && e.target.closest('.msg__tool[data-act="react"]'))) {
      closePopover();
    }
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && openPopover) closePopover();
  });

  // --- tape helpers --------------------------------------------------------------

  function scrollTapeBottom(instant) {
    if (reduceMotion || instant || !tape.scrollTo) {
      tape.scrollTop = tape.scrollHeight;
    } else {
      tape.scrollTo({ top: tape.scrollHeight, behavior: "smooth" });
    }
    hideCueTab();
  }
  var NEAR_BOTTOM = 80;
  function nearBottom() {
    return tape.scrollHeight - tape.scrollTop - tape.clientHeight < NEAR_BOTTOM;
  }

  var cueTab = null;
  function ensureCueTab() {
    if (cueTab || !cue) return cueTab;
    cueTab = el("button", "cue-tab", "Jump to latest");
    cueTab.type = "button";
    cueTab.hidden = true;
    cueTab.addEventListener("click", function () { scrollTapeBottom(false); });
    cue.insertBefore(cueTab, cue.firstChild);
    return cueTab;
  }
  function showCueTab(newCount) {
    var b = ensureCueTab();
    if (!b) return;
    b.hidden = false;
    b.textContent = newCount > 0 ? newCount + " new — jump to latest" : "Jump to latest";
    b.__new = newCount;
  }
  function hideCueTab() {
    if (!cueTab) return;
    cueTab.hidden = true;
    cueTab.__new = 0;
  }
  tape.addEventListener("scroll", function () {
    if (nearBottom()) hideCueTab();
  });

  function clearTapeRows() {
    var rows = tape.querySelectorAll(".msg, .msg--skel, .timeline__empty, .tape__empty");
    for (var i = 0; i < rows.length; i++) rows[i].remove();
  }
  function showSkeleton() {
    clearTapeRows();
    var newest = tape.querySelector(".tape__splice--newest");
    for (var i = 0; i < 4; i++) {
      var skel = el("li", "msg--skel");
      skel.setAttribute("aria-hidden", "true");
      skel.appendChild(el("span"));
      skel.appendChild(el("span"));
      tape.insertBefore(skel, newest);
    }
    if (tapeStatus) tapeStatus.textContent = "Loading…";
  }
  function setTapeStatus(text) {
    if (tapeStatus) tapeStatus.textContent = text;
  }
  function appendRow(row) {
    var empty = tape.querySelector(".timeline__empty, .tape__empty");
    if (empty) empty.remove();
    var newest = tape.querySelector(".tape__splice--newest");
    tape.insertBefore(row, newest);
  }

  // Focus + flash a message that is already in the loaded window. This is the whole locator:
  // v1 can only locate inside the latest window; anything else navigates to the SSR locator.
  function locateRow(msgId) {
    var row = rowFor(msgId);
    if (!row) return false;
    selectionEpoch++;
    row.setAttribute("tabindex", "-1");
    row.classList.add("is-located", "msg--flash");
    row.focus({ preventScroll: true });
    row.scrollIntoView({ block: "center", behavior: reduceMotion ? "auto" : "smooth" });
    setTimeout(function () { row.classList.remove("msg--flash"); }, reduceMotion ? 0 : 1200);
    return true;
  }

  // --- read marker ----------------------------------------------------------------
  // The server owns the timestamp: we send only the message id, and marks clear only after
  // a persisted response.

  var roomActivityClock = 0;
  var roomActivityGeneration = Object.create(null);
  var roomReadSequence = Object.create(null);

  function currentRoomActivity(roomId) {
    return roomActivityGeneration[roomId] || 0;
  }

  function noteRoomActivity(roomId) {
    roomActivityClock++;
    roomActivityGeneration[roomId] = roomActivityClock;
  }

  function newestRowId() {
    var rows = tape.querySelectorAll("[data-msg-id]");
    if (!rows.length) return null;
    return rows[rows.length - 1].getAttribute("data-msg-id");
  }

  function markRead(roomId, messageId) {
    if (!roomId || !messageId) return;
    var ep = currentEpoch();
    var activity = currentRoomActivity(roomId);
    var readSequence = (roomReadSequence[roomId] || 0) + 1;
    roomReadSequence[roomId] = readSequence;
    fetch("/api/rooms/" + encodeURIComponent(roomId) + "/read", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ message_id: messageId })
    })
      .then(function (r) {
        if (r.ok && !stale(ep) && selected === roomId &&
            roomReadSequence[roomId] === readSequence &&
            currentRoomActivity(roomId) === activity) {
          clearUnread(roomId);
        }
      })
      .catch(function () { /* marks stay — the truth simply has not advanced */ });
  }

  // --- jack badges (square unread tick / diamond mention — never count-exact) --------

  function jackRow(roomId) {
    return jackList ? jackList.querySelector('[data-room-id="' + cssEscape(roomId) + '"]') : null;
  }
  function clearUnread(roomId) {
    var li = jackRow(roomId);
    if (!li) return;
    var unread = li.querySelector(".room__unread, .jack__mark--unread");
    if (unread) {
      if (unread.classList.contains("room__unread")) unread.hidden = true;
      else unread.remove();
    }
    var mention = li.querySelector(".room__mention, .jack__mark--mention");
    if (mention) mention.remove();
  }
  function bumpUnread(roomId, mention) {
    var li = jackRow(roomId);
    if (!li) return;
    var unread = li.querySelector(".room__unread, .jack__mark--unread");
    if (unread) {
      unread.hidden = false;
    } else {
      var mark = el("span", "jack__mark--unread", "Unread");
      var link = li.querySelector("a.jack__link") || li;
      link.appendChild(mark);
    }
    if (mention && !li.querySelector(".room__mention, .jack__mark--mention")) {
      var diamond = el("span", "jack__mark--mention");
      diamond.setAttribute("aria-label", "Mentions you");
      var host = li.querySelector("a.jack__link") || li;
      host.appendChild(diamond);
    }
  }

  // --- room selection ---------------------------------------------------------------

  var roomFetch = null; // AbortController for the in-flight room switch

  function syncSelectedAuthority(room) {
    var archived = !!(room && room.archived);
    selectedAuthority = archived ? "archived" : "active";
    var row = jackRow(selected);
    if (row) row.setAttribute("data-room-state", archived ? "archived" : "active");
    deck.classList.toggle("deck--archived", archived);
    deck.classList.remove("deck--unavailable");
    if (archived) {
      if (editingId) cancelEdit();
      clearReply();
    }
    setComposerEnabled(selectedAuthority === "active" && !sending);
  }

  function markSelectedUnavailable() {
    selectedAuthority = "unavailable";
    var row = jackRow(selected);
    if (row) row.setAttribute("data-room-state", "unavailable");
    deck.classList.remove("deck--archived");
    deck.classList.add("deck--unavailable");
    if (editingId) cancelEdit();
    else clearReply();
    setComposerEnabled(false);
    clearTapeRows();
    setTapeStatus("Resource unavailable");
    if (roomTopic) roomTopic.textContent = "";
  }

  function selectRoom(id, opts) {
    opts = opts || {};
    if (!id) return;
    selectionEpoch++;
    var ep = currentEpoch();
    selected = id;

    // Jack geometry: aria-current + legacy is-active both follow the selection.
    if (jackList) {
      var rows = jackList.querySelectorAll("[data-room-id]");
      for (var i = 0; i < rows.length; i++) {
        var match = rows[i].getAttribute("data-room-id") === id;
        rows[i].classList.toggle("is-active", match);
        var link = rows[i].querySelector("a.jack__link") || rows[i];
        if (rows[i].hasAttribute("aria-current") || link.hasAttribute("aria-current")) {
          if (match) link.setAttribute("aria-current", "page");
          else link.removeAttribute("aria-current");
          rows[i].removeAttribute("aria-current");
        }
        if (match) {
          selectedKind = rows[i].getAttribute("data-room-kind") || "room";
          var authorityToken = rows[i].getAttribute("data-room-state");
          selectedAuthority =
            authorityToken === "archived" ? "archived" :
            authorityToken === "active" ? "active" : "unavailable";
        }
      }
    }

    var name = opts.name || nameForRoom(id) || id;
    if (roomTitle) roomTitle.textContent = name;
    if (patchbarCurrent) patchbarCurrent.textContent = name;
    if (cueRoom) cueRoom.textContent = name;
    deck.classList.toggle("deck--archived", selectedAuthority === "archived");
    deck.classList.toggle("deck--unavailable", selectedAuthority === "unavailable");
    if (roomTopic) roomTopic.textContent = "";
    if (cue) cue.setAttribute("action", "/api/rooms/" + encodeURIComponent(id) + "/messages");
    if (ledgerRoom) ledgerRoom.value = id;
    ["search", "mentions", "pins"].forEach(function (tab) {
      var a = document.getElementById("pb-tab-" + tab);
      if (a) a.setAttribute("href", "/?room=" + encodeURIComponent(id) + "&ledger=" + tab);
    });
    if (editingId) cancelEdit();
    else clearReply();
    clearReceipt();
    setComposerEnabled(selectedAuthority === "active");
    restoreDraft(id);
    showSkeleton();
    closeJackDrawer();

    if (roomFetch) roomFetch.abort();
    roomFetch = new AbortController();

    fetch("/api/rooms/" + encodeURIComponent(id) + "/messages", {
      credentials: "same-origin",
      signal: roomFetch.signal
    })
      .then(function (r) {
        if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
        return r.json();
      })
      .then(function (data) {
        if (stale(ep)) return; // a newer selection owns the DOM now
        clearTapeRows();
        var msgs = (data.messages || []).slice().reverse(); // server returns newest-first
        if (msgs.length === 0) {
          setTapeStatus("No messages in the latest window");
        } else {
          setTapeStatus("Latest window · up to 50 messages");
          for (var i = 0; i < msgs.length; i++) appendRow(buildRow(msgs[i]));
        }
        scrollTapeBottom(true);
        markRead(id, newestRowId());
        if (ledgerOpenTab) loadLedger(ledgerOpenTab, { keepOpen: true });
      })
      .catch(function (err) {
        if (stale(ep) || (err && err.name === "AbortError")) return;
        clearTapeRows();
        setTapeStatus("Tape unavailable — reload to retry");
      });

    // Room metadata (topic) rides the rooms list; failure only leaves the topic blank.
    fetch("/api/rooms", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (stale(ep) || !data) return;
        var rooms = data.rooms || [];
        for (var i = 0; i < rooms.length; i++) {
          var room = rooms[i].room || rooms[i];
          if (room && room.id === id && roomTopic) {
            roomTopic.textContent = room.topic || "";
            break;
          }
        }
      })
      .catch(function () {});
  }

  function nameForRoom(id) {
    var li = jackRow(id);
    if (!li) return null;
    var label = li.querySelector(".jack__label");
    if (label) return label.textContent;
    var btn = li.querySelector(".room__btn");
    if (btn) return btn.textContent;
    return li.getAttribute("data-room-name");
  }

  if (jackList) {
    jackList.addEventListener("click", function (e) {
      var row = e.target.closest ? e.target.closest("[data-room-id]") : null;
      if (!row) return;
      var id = row.getAttribute("data-room-id");
      if (!id || id === selected) { closeJackDrawer(); e.preventDefault(); return; }
      e.preventDefault(); // JS enhances the real anchor into an in-place switch
      selectRoom(id);
    });
  }

  // --- composer: receipt, budget, draft, IME-safe send -------------------------------

  function setComposerEnabled(enabled) {
    if (cueInput) cueInput.disabled = !enabled;
    if (cueSend) cueSend.disabled = !enabled || sending;
  }

  var sending = false;
  function setReceipt(text, isError) {
    if (!receipt) return;
    receipt.textContent = text || "";
    receipt.classList.toggle("is-error", !!isError);
  }
  function clearReceipt() { setReceipt("", false); }

  function draftKey(roomId) {
    return "murmur.draft." + roomId + "." + (replyField && replyField.value ? replyField.value : "");
  }
  function saveDraft() {
    if (!cueInput || !selected) return;
    try {
      if (cueInput.value) sessionStorage.setItem(draftKey(selected), cueInput.value);
      else sessionStorage.removeItem(draftKey(selected));
    } catch (e) { /* sessionStorage unavailable — drafts are a convenience only */ }
  }
  function restoreDraft(roomId) {
    if (!cueInput) return;
    var value = "";
    try { value = sessionStorage.getItem(draftKey(roomId)) || ""; } catch (e) { value = ""; }
    cueInput.value = value;
    autosize();
    updateBudget();
  }

  function autosize() {
    if (!cueInput) return;
    cueInput.style.height = "auto";
    cueInput.style.height = Math.min(cueInput.scrollHeight, 220) + "px";
  }
  function updateBudget() {
    if (!cueBudget || !cueInput) return;
    var n = cueInput.value.length;
    cueBudget.textContent = n.toLocaleString("en-US") + " / " + BODY_MAX.toLocaleString("en-US");
  }

  if (cueInput) {
    cueInput.addEventListener("input", function () {
      autosize();
      updateBudget();
      saveDraft();
    });
    cueInput.addEventListener("keydown", function (e) {
      // IME guard: Enter mid-composition confirms the candidate, it must never send.
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        sendMessage();
      }
    });
  }

  if (cue) {
    cue.addEventListener("submit", function (e) {
      e.preventDefault();
      sendMessage();
    });
  }

  function sendMessage() {
    if (!cueInput || sending) return;
    if (selectedAuthority !== "active" || !selected) return;
    if (cueInput.disabled) return;
    var body = cueInput.value.trim();
    if (!body) return;

    if (editingId) { submitEdit(body); return; }

    sending = true;
    setComposerEnabled(false);
    setReceipt("Sending…", false);
    var payload = { body: body };
    if (replyField && replyField.value) payload.reply_to_id = replyField.value;
    var requestRoom = selected;
    var sentDraftKey = draftKey(requestRoom);
    var ep = currentEpoch();

    fetch("/api/rooms/" + encodeURIComponent(requestRoom) + "/messages", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify(payload)
    })
      .then(function (r) {
        if (r.status === 201) return r.json();
        return parseErrorResponse(r).then(function (copy) {
          throw { receipt: "rejected", copy: copy };
        });
      })
      .then(function (data) {
        var ok = data && data.message && data.message.id &&
          data.receipt && data.receipt.state === "persisted" &&
          data.receipt.message_id === data.message.id;
        if (!ok) throw { receipt: "unknown" };
        // Persisted: clear only now, render the canonical message immediately, and let a
        // later live echo replace the same row in place (dedupe by data-msg-id).
        try { sessionStorage.removeItem(sentDraftKey); } catch (e) {}
        if (stale(ep) || selected !== requestRoom) return;
        if (cueInput) {
          cueInput.value = "";
          autosize();
          updateBudget();
        }
        clearReply();
        appendRow(buildRow(data.message));
        scrollTapeBottom(true);
        markRead(requestRoom, data.message.id);
        setReceipt("Saved · " + fmtTime(data.receipt.created_at || data.message.created_at) + " UTC", false);
      })
      .catch(function (err) {
        if (stale(ep) || selected !== requestRoom) return;
        if (err && err.receipt === "rejected") {
          setReceipt("Not sent — " + err.copy, true); // body stays in the composer
        } else {
          setReceipt("Send uncertain — your text is preserved; check the tape before retrying", true);
        }
      })
      .then(function () {
        sending = false;
        setComposerEnabled(selectedAuthority === "active");
      });
  }

  // --- reply / edit / delete ----------------------------------------------------------

  function setReply(id, label) {
    if (!replyField || !loopText) return;
    replyField.value = id;
    loopText.textContent = label || ("Loop to message " + id);
    if (loopCancel) loopCancel.setAttribute("href", "/?room=" + encodeURIComponent(selected));
    if (cueInput) cueInput.focus();
  }
  function clearReply() {
    if (replyField) replyField.value = "";
    if (loopText) loopText.textContent = "";
  }
  if (loopCancel) {
    loopCancel.addEventListener("click", function (e) {
      e.preventDefault();
      clearReply();
      if (cueInput) cueInput.focus();
    });
  }

  var editingId = null;
  var editReturnFocus = null;
  function startEdit(row, toolBtn) {
    cancelEdit();
    var bodyEl = row.querySelector(".msg__body");
    if (!bodyEl || !cueInput) return;
    editingId = row.getAttribute("data-msg-id");
    editReturnFocus = toolBtn || null;
    cueInput.value = bodyEl.textContent;
    autosize();
    updateBudget();
    if (loopText) loopText.textContent = "Editing message";
    setReceipt("", false);
    cueInput.focus();
  }
  function cancelEdit() {
    if (!editingId) return;
    editingId = null;
    if (cueInput) { cueInput.value = ""; autosize(); updateBudget(); }
    if (loopText) loopText.textContent = "";
    if (editReturnFocus) { editReturnFocus.focus(); editReturnFocus = null; }
  }
  function submitEdit(body) {
    var id = editingId;
    var requestRoom = selected;
    var ep = currentEpoch();
    sending = true;
    setComposerEnabled(false);
    setReceipt("Sending…", false);
    fetch("/api/rooms/" + encodeURIComponent(requestRoom) + "/messages/" +
      encodeURIComponent(id) + "/edit", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ body: body })
    })
      .then(function (r) {
        if (!r.ok) return parseErrorResponse(r).then(function (copy) {
          throw { rejected: true, copy: copy };
        });
        return r.json();
      })
      .then(function (data) {
        if (stale(ep) || selected !== requestRoom) return;
        if (!data || !data.message) throw { rejected: true, copy: "edit result unavailable" };
        var old = rowFor(id);
        if (old) old.parentNode.replaceChild(buildRow(data.message), old);
        editingId = null;
        if (cueInput) { cueInput.value = ""; autosize(); updateBudget(); }
        if (loopText) loopText.textContent = "";
        setReceipt("Saved · " + fmtTime(data.message.edited_at || 0) + " UTC", false);
      })
      .catch(function (err) {
        if (stale(ep) || selected !== requestRoom) return;
        setReceipt("Not sent — " + (err && err.copy ? err.copy : "edit failed"), true);
      })
      .then(function () {
        sending = false;
        setComposerEnabled(selectedAuthority === "active");
      });
  }

  // Two-stage inline delete — a real second deliberate action, never confirm().
  function armDelete(row, toolBtn) {
    var host = toolBtn.parentNode;
    if (!host || host.querySelector(".msg__confirm")) return;
    toolBtn.hidden = true;
    var wrap = el("span", "msg__confirm");
    wrap.appendChild(el("span", "msg__confirm-label", "Delete this message?"));
    var yes = el("button", "msg__tool msg__tool--danger", "Delete permanently");
    yes.type = "button";
    var no = el("button", "msg__tool", "Keep");
    no.type = "button";
    function disarm() {
      wrap.remove();
      toolBtn.hidden = false;
      toolBtn.focus();
    }
    no.addEventListener("click", disarm);
    yes.addEventListener("click", function () {
      yes.disabled = true;
      var id = row.getAttribute("data-msg-id");
      var requestRoom = selected;
      var ep = currentEpoch();
      fetch("/api/rooms/" + encodeURIComponent(requestRoom) + "/messages/" +
        encodeURIComponent(id) + "/delete", {
        method: "POST",
        headers: apiHeaders(false),
        credentials: "same-origin"
      })
        .then(function (r) {
          if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
          return r.json();
        })
        .then(function (data) {
          if (stale(ep) || selected !== requestRoom) return;
          if (data && data.message) {
            row.parentNode.replaceChild(buildRow(data.message), row);
          } else {
            row.setAttribute("data-lifecycle", "deleted");
            wrap.remove();
          }
        })
        .catch(function (err) {
          if (stale(ep) || selected !== requestRoom) return;
          yes.disabled = false;
          rowNote(row, err && err.message ? err.message : "Delete failed");
        });
    });
    wrap.appendChild(yes);
    wrap.appendChild(no);
    host.appendChild(wrap);
    no.focus();
  }

  // --- tape event delegation ---------------------------------------------------------

  tape.addEventListener("click", function (e) {
    var chip = e.target.closest ? e.target.closest(".reaction") : null;
    if (chip) {
      // Archived rooms are read-only: chips are inert tallies, never a mutation.
      if (selectedAuthority !== "active") return;
      var chipRow = chip.closest("[data-msg-id]");
      if (chipRow) toggleReaction(chipRow.getAttribute("data-msg-id"), chip.getAttribute("data-emoji"));
      return;
    }
    var locate = e.target.closest ? e.target.closest("[data-locate]") : null;
    if (locate) {
      var target = locate.getAttribute("data-locate");
      if (rowFor(target)) {
        e.preventDefault();
        locateRow(target);
      }
      return; // otherwise the real anchor navigates to the SSR locator
    }
    var tool = e.target.closest ? e.target.closest(".msg__tool") : null;
    if (!tool) return;
    var row = tool.closest("[data-msg-id]");
    if (!row) return;
    var id = row.getAttribute("data-msg-id");
    var act = tool.getAttribute("data-act");
    if (act === "reply") {
      var authorEl = row.querySelector(".msg__author");
      var bodyEl = row.querySelector(".msg__body");
      var snippet = bodyEl ? bodyEl.textContent.replace(/[\r\n]+/g, " ").trim() : "";
      if (snippet.length > 120) snippet = snippet.slice(0, 120) + "…";
      setReply(id, "Loop to " + (authorEl ? authorEl.textContent : "message") +
        (snippet ? " · " + snippet : ""));
    } else if (act === "react") {
      openReactPopover(tool, id);
    } else if (act === "pin") {
      pinMessage(id);
    } else if (act === "edit") {
      startEdit(row, tool);
    } else if (act === "delete") {
      armDelete(row, tool);
    }
  });

  // Escape cancels an in-progress reply / edit and returns focus to the trigger.
  if (cueInput) {
    cueInput.addEventListener("keydown", function (e) {
      if (e.key !== "Escape") return;
      if (editingId) { cancelEdit(); }
      else if (replyField && replyField.value) { clearReply(); }
    });
  }

  function pinMessage(msgId) {
    var requestRoom = selected;
    var ep = currentEpoch();
    fetch("/api/rooms/" + encodeURIComponent(requestRoom) + "/messages/" +
      encodeURIComponent(msgId) + "/pin", {
      method: "POST",
      headers: apiHeaders(false),
      credentials: "same-origin"
    })
      .then(function (r) {
        if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
        if (stale(ep) || selected !== requestRoom) return;
        var row = rowFor(msgId);
        if (row) rowNote(row, "Pinned");
        if (ledgerOpenTab === "pins") loadLedger("pins", { keepOpen: true });
      })
      .catch(function (err) {
        if (stale(ep) || selected !== requestRoom) return;
        var row = rowFor(msgId);
        if (row) rowNote(row, err && err.message ? err.message : "Pin failed");
      });
  }

  // --- Patch Ledger ----------------------------------------------------------------------

  var ledgerOpenTab = null;
  var ledgerEpoch = 0;

  function setLedgerTab(tab) {
    ["search", "mentions", "pins"].forEach(function (name) {
      var a = document.getElementById("pb-tab-" + name);
      if (!a) return;
      if (name === tab) a.setAttribute("aria-current", "true");
      else a.removeAttribute("aria-current");
    });
  }

  function ledgerStatus(text) {
    var status = ledgerResults ? ledgerResults.querySelector(".pb-ledger__status") : null;
    if (!status && ledgerResults) {
      status = el("p", "pb-ledger__status");
      ledgerResults.insertBefore(status, ledgerResults.firstChild);
    }
    if (status) status.textContent = text;
  }

  function clearLedgerItems() {
    if (!ledgerResults) return;
    var items = ledgerResults.querySelectorAll(".pb-ledger__item");
    for (var i = 0; i < items.length; i++) items[i].remove();
  }

  function ledgerItem(roomId, roomName, hit) {
    var a = el("a", "pb-ledger__item");
    a.setAttribute("href", "/?room=" + encodeURIComponent(roomId) +
      "&message=" + encodeURIComponent(hit.id) + "#msg-" + encodeURIComponent(hit.id));
    a.setAttribute("data-locate-room", roomId);
    a.setAttribute("data-locate", hit.id);
    a.appendChild(el("span", "", roomName || roomId));
    a.appendChild(el("b", "", (hit.sender_email || hit.sender_sub || "—") +
      " · " + fmtTime(hit.created_at || 0)));
    var body = hit.deleted ? "[deleted]" : String(hit.body || "").replace(/[\r\n]+/g, " ");
    if (body.length > 140) body = body.slice(0, 140) + "…";
    a.appendChild(el("span", "", body));
    return a;
  }

  function loadLedger(tab, opts) {
    opts = opts || {};
    if (!ledgerResults) return;
    ledgerOpenTab = tab;
    setLedgerTab(tab);
    ledgerEpoch++;
    var requestLedgerEpoch = ledgerEpoch;
    var ep = currentEpoch();
    var requestTab = tab;
    var requestRoom = selected;
    var requestQuery = "";
    var requestRoomName = roomTitle ? roomTitle.textContent : requestRoom;
    function ledgerRequestIsStale() {
      if (stale(ep) || requestLedgerEpoch !== ledgerEpoch || ledgerOpenTab !== requestTab) {
        return true;
      }
      if (requestTab === "pins" && selected !== requestRoom) return true;
      return requestTab === "search" && ledgerQ && ledgerQ.value.trim() !== requestQuery;
    }
    var url;
    if (tab === "search") {
      requestQuery = ledgerQ ? ledgerQ.value.trim() : "";
      if (requestQuery.length < SEARCH_MIN) {
        clearLedgerItems();
        ledgerStatus("Enter at least " + SEARCH_MIN + " characters to search");
        return;
      }
      url = "/api/search?q=" + encodeURIComponent(requestQuery);
    } else if (tab === "mentions") {
      url = "/api/mentions";
    } else {
      url = "/api/rooms/" + encodeURIComponent(requestRoom) + "/pinned";
    }
    ledgerStatus("Loading…");
    fetch(url, { credentials: "same-origin" })
      .then(function (r) {
        if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
        return r.json();
      })
      .then(function (data) {
        if (ledgerRequestIsStale()) return;
        clearLedgerItems();
        var rows = [];
        if (tab === "pins") {
          (data.messages || []).forEach(function (m) {
            rows.push(ledgerItem(requestRoom, requestRoomName, m));
          });
        } else {
          (data.results || []).forEach(function (hit) {
            rows.push(ledgerItem(hit.room_id, hit.room_name, hit));
          });
        }
        if (rows.length === 0) {
          ledgerStatus(tab === "search"
            ? "No matches in rooms you can read"
            : "No results in this bounded view");
          return;
        }
        ledgerStatus("Showing up to 50 results");
        rows.forEach(function (row) { ledgerResults.appendChild(row); });
      })
      .catch(function (err) {
        if (ledgerRequestIsStale()) return;
        clearLedgerItems();
        ledgerStatus((err && err.message ? err.message : "Unavailable") + " — retry from the form above");
      });
  }

  ["search", "mentions", "pins"].forEach(function (tab) {
    var a = document.getElementById("pb-tab-" + tab);
    if (!a) return;
    a.addEventListener("click", function (e) {
      e.preventDefault();
      openLedger();
      if (tab === "search" && ledgerQ && !ledgerQ.value.trim()) {
        setLedgerTab("search");
        ledgerOpenTab = "search";
        ledgerQ.focus();
        return;
      }
      loadLedger(tab);
    });
  });

  if (ledgerSearch) {
    ledgerSearch.addEventListener("submit", function (e) {
      e.preventDefault();
      openLedger();
      loadLedger("search");
    });
  }

  // Locate from the ledger: same room + row already in the window → focus in place;
  // anything else falls through to the real SSR locator navigation.
  if (ledgerResults) {
    ledgerResults.addEventListener("click", function (e) {
      var item = e.target.closest ? e.target.closest("[data-locate]") : null;
      if (!item) return;
      var roomId = item.getAttribute("data-locate-room");
      var msgId = item.getAttribute("data-locate");
      if ((!roomId || roomId === selected) && rowFor(msgId)) {
        e.preventDefault();
        locateRow(msgId);
        if (narrowMq && narrowMq.matches) closeLedger();
      }
    });
  }

  function ledgerIsRail() { return ledgerRailMq && ledgerRailMq.matches; }
  var ledgerScrim = null;
  // Below 1024px the ledger opens as a genuine modal sheet (right sheet / bottom sheet)
  // with scrim, focus trap, Escape, and an explicit Close — it never half-covers the cue.
  function openLedger() {
    if (!ledger || ledgerIsRail()) return;
    ledger.classList.add("is-open");
    ledger.setAttribute("role", "dialog");
    ledger.setAttribute("aria-modal", "true");
    if (ledgerToggle) ledgerToggle.setAttribute("aria-expanded", "true");
    if (!ledgerScrim) {
      ledgerScrim = el("button", "ledger-scrim");
      ledgerScrim.type = "button";
      ledgerScrim.setAttribute("aria-label", "Close ledger");
      ledgerScrim.addEventListener("click", closeLedger);
      document.body.appendChild(ledgerScrim);
    }
    trapFocus(ledger, closeLedger);
    var firstTab = document.getElementById("pb-tab-search");
    if (firstTab) firstTab.focus();
  }
  function closeLedger() {
    if (!ledger || !ledger.classList.contains("is-open")) return;
    ledger.classList.remove("is-open");
    ledger.removeAttribute("role");
    ledger.removeAttribute("aria-modal");
    if (ledgerToggle) ledgerToggle.setAttribute("aria-expanded", "false");
    if (ledgerScrim) { ledgerScrim.remove(); ledgerScrim = null; }
    releaseTrap(ledger);
    if (ledgerToggle) ledgerToggle.focus();
  }
  if (ledgerToggle) {
    ledgerToggle.addEventListener("click", function () {
      if (ledger.classList.contains("is-open")) closeLedger();
      else openLedger();
    });
  }
  if (ledgerClose) ledgerClose.addEventListener("click", closeLedger);

  // --- focus containment for drawer / sheet / dialog -------------------------------------

  var traps = [];
  function trapFocus(container, onEscape) {
    releaseTrap(container);
    var trap = { container: container, onEscape: onEscape, last: document.activeElement };
    trap.keyHandler = function (e) {
      if (e.key === "Escape") {
        e.stopPropagation();
        trap.onEscape();
        return;
      }
      if (e.key !== "Tab") return;
      var focusables = container.querySelectorAll(
        'a[href], button:not([disabled]), input:not([disabled]), textarea:not([disabled]), [tabindex="-1"]');
      if (!focusables.length) return;
      var first = focusables[0];
      var last = focusables[focusables.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", trap.keyHandler, true);
    traps.push(trap);
  }
  function releaseTrap(container) {
    for (var i = traps.length - 1; i >= 0; i--) {
      if (traps[i].container === container) {
        document.removeEventListener("keydown", traps[i].keyHandler, true);
        if (traps[i].last && traps[i].last.focus) traps[i].last.focus();
        traps.splice(i, 1);
      }
    }
  }

  // --- jackfield drawer (<768px, JS only) --------------------------------------------------

  var jackScrim = null;
  function openJackDrawer() {
    if (!jackfield || (narrowMq && !narrowMq.matches)) return;
    jackfield.classList.add("is-open");
    jackfield.setAttribute("role", "dialog");
    jackfield.setAttribute("aria-modal", "true");
    jackfield.setAttribute("aria-label", "Rooms");
    if (roomsOpenBtn) roomsOpenBtn.setAttribute("aria-expanded", "true");
    if (!jackScrim) {
      jackScrim = el("button", "jackfield-scrim");
      jackScrim.type = "button";
      jackScrim.setAttribute("aria-label", "Close rooms");
      jackScrim.addEventListener("click", closeJackDrawer);
      document.body.appendChild(jackScrim);
    }
    trapFocus(jackfield, closeJackDrawer);
    var current = jackfield.querySelector('[aria-current="page"], .is-active a, .is-active button');
    var toFocus = current || jackfield.querySelector("a, button");
    if (toFocus) toFocus.focus();
  }
  function closeJackDrawer() {
    if (!jackfield || !jackfield.classList.contains("is-open")) return;
    jackfield.classList.remove("is-open");
    jackfield.removeAttribute("role");
    jackfield.removeAttribute("aria-modal");
    if (roomsOpenBtn) roomsOpenBtn.setAttribute("aria-expanded", "false");
    if (jackScrim) { jackScrim.remove(); jackScrim = null; }
    releaseTrap(jackfield);
    if (roomsOpenBtn) roomsOpenBtn.focus();
  }
  if (roomsOpenBtn) {
    roomsOpenBtn.addEventListener("click", function () {
      if (jackfield.classList.contains("is-open")) closeJackDrawer();
      else openJackDrawer();
    });
  }
  function resetOverlaysForWidth() {
    // Leaving a modal breakpoint drops every modal artefact; the regions re-open cleanly.
    if (narrowMq && !narrowMq.matches) closeJackDrawer();
    if (ledgerIsRail()) closeLedger();
  }
  if (narrowMq && narrowMq.addEventListener) narrowMq.addEventListener("change", resetOverlaysForWidth);
  if (ledgerRailMq && ledgerRailMq.addEventListener) ledgerRailMq.addEventListener("change", resetOverlaysForWidth);

  // --- new room / new DM dialogs -------------------------------------------------------------

  function openDialog(titleText, buildBody) {
    var overlay = el("div", "pb-dialog");
    var panel = el("div", "pb-dialog__panel");
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    var titleId = "pb-dialog-title-" + (++dialogSeq);
    panel.setAttribute("aria-labelledby", titleId);

    var head = el("div", "pb-dialog__head");
    var title = el("h2", "pb-dialog__title", titleText);
    title.id = titleId;
    head.appendChild(title);
    var closeBtn = el("button", "pb-dialog__close", "Close");
    closeBtn.type = "button";
    head.appendChild(closeBtn);
    panel.appendChild(head);

    var body = el("div", "pb-dialog__body");
    // buildBody runs synchronously; ctl.close is wired before any async submit can fire.
    var ctl = {};
    buildBody(body, ctl);
    panel.appendChild(body);
    overlay.appendChild(panel);

    ctl.close = function () {
      releaseTrap(panel);
      overlay.remove();
    };
    closeBtn.addEventListener("click", ctl.close);
    overlay.addEventListener("click", function (e) { if (e.target === overlay) ctl.close(); });
    document.body.appendChild(overlay);
    trapFocus(panel, ctl.close); // trap handles Escape via onEscape
    var first = body.querySelector("input, button");
    if (first) first.focus();
    return ctl;
  }
  var dialogSeq = 0;

  if (newRoomBtn) {
    newRoomBtn.hidden = false;
    newRoomBtn.addEventListener("click", function () {
      var input;
      openDialog("New room", function (body, ctl) {
        body.appendChild(el("label", "pb-dialog__label", "Room name")).setAttribute("for", "pb-new-room-name");
        input = el("input", "pb-dialog__input");
        input.id = "pb-new-room-name";
        input.type = "text";
        input.maxLength = 120;
        input.required = true;
        body.appendChild(input);
        var err = el("p", "pb-dialog__error");
        err.hidden = true;
        err.id = "pb-new-room-error";
        body.appendChild(err);
        var actions = el("div", "pb-dialog__actions");
        var create = el("button", "pb-dialog__submit", "Create");
        create.type = "button";
        create.addEventListener("click", submit);
        actions.appendChild(create);
        body.appendChild(actions);
        input.addEventListener("keydown", function (e) {
          if (e.key === "Enter") { e.preventDefault(); submit(); }
          e.stopPropagation();
        });
        function submit() {
          var name = input.value.trim();
          if (!name) {
            err.textContent = "Name the room first";
            err.hidden = false;
            input.focus();
            return;
          }
          create.disabled = true;
          fetch("/api/rooms", {
            method: "POST",
            headers: apiHeaders(true),
            credentials: "same-origin",
            body: JSON.stringify({ name: name, kind: "room" })
          })
            .then(function (r) {
              if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
              return r.json();
            })
            .then(function (data) {
              if (!data || !data.room) throw new Error("Action not allowed");
              ctl.close();
              addJack(data.room);
              selectRoom(data.room.id, { name: data.room.name });
              reconnect(); // the socket snapshots membership at connect time
            })
            .catch(function (e2) {
              create.disabled = false;
              err.textContent = e2 && e2.message ? e2.message : "Could not create the room";
              err.hidden = false;
            });
        }
      });
    });
  }

  if (newDmBtn) {
    newDmBtn.hidden = false;
    newDmBtn.addEventListener("click", function () {
      fetch("/api/directory", { credentials: "same-origin" })
        .then(function (r) { return r.ok ? r.json() : { people: [] }; })
        .then(function (data) {
          var people = data.people || [];
          openDialog("New direct message", function (body, ctl) {
            if (people.length === 0) {
              body.appendChild(el("p", "pb-dialog__empty", "No people to message yet."));
              return;
            }
            body.appendChild(el("p", "pb-dialog__empty",
              "Display names come from the directory and are not verified."));
            var list = el("ul", "pb-dialog__list");
            people.forEach(function (person) {
              var li = el("li");
              var b = el("button", "pb-dialog__person", person.user_email || person.user_sub || "—");
              b.type = "button";
              b.addEventListener("click", function () {
                b.disabled = true;
                fetch("/api/dms", {
                  method: "POST",
                  headers: apiHeaders(true),
                  credentials: "same-origin",
                  body: JSON.stringify({ subject: person.user_sub, email: person.user_email })
                })
                  .then(function (r) {
                    if (!r.ok) return parseErrorResponse(r).then(function (copy) { throw new Error(copy); });
                    return r.json();
                  })
                  .then(function (d2) {
                    if (!d2 || !d2.room) throw new Error("Action not allowed");
                    ctl.close();
                    addJack(d2.room);
                    selectRoom(d2.room.id, { name: d2.room.name });
                    reconnect();
                  })
                  .catch(function () { b.disabled = false; });
              });
              li.appendChild(b);
              list.appendChild(li);
            });
            body.appendChild(list);
          });
        })
        .catch(function () {});
    });
  }

  // Insert a freshly joined room into the jackfield using the frozen jack markup.
  function addJack(room) {
    if (!jackList || !room || !room.id) return;
    if (jackRow(room.id)) return;
    var li = el("li", "jack");
    var a = el("a", "jack__link");
    a.setAttribute("href", "/?room=" + encodeURIComponent(room.id));
    a.setAttribute("data-room-id", room.id);
    a.setAttribute("data-room-kind", room.kind === "dm" ? "dm" : "room");
    a.setAttribute("data-room-state", room.archived ? "archived" : "active");
    a.appendChild(el("span", "jack__ring")).setAttribute("aria-hidden", "true");
    a.appendChild(el("span", "jack__label", room.name || room.id));
    li.appendChild(a);
    jackList.appendChild(li);
  }

  // --- presence: transient cues only, never a roster ----------------------------------------

  function presenceCue(text) {
    if (!presenceHost) return;
    var cueEl = el("div", "pb-presence__cue", text);
    presenceHost.appendChild(cueEl);
    setTimeout(function () { if (cueEl.parentNode) cueEl.parentNode.removeChild(cueEl); }, 4000);
  }

  // --- coalesced polite announcements (single live region) ------------------------------------

  var announceTimer = null;
  var announceQueue = [];
  function announce(text) {
    if (!livePolite) return;
    announceQueue.push(text);
    if (announceTimer) return;
    announceTimer = setTimeout(function () {
      var last = announceQueue[announceQueue.length - 1];
      var extra = announceQueue.length - 1;
      livePolite.textContent = extra > 0 ? last + " (" + extra + " more updates)" : last;
      announceQueue = [];
      announceTimer = null;
    }, 2000);
  }

  // --- transport meter + live socket -------------------------------------------------------------
  // Unknown → Connected → Reconnecting (bounded backoff) → Offline (manual Reconnect).
  // "Catching up" is an orthogonal reconcile flag, not a transport state.

  var ws = null;
  var retry = 0;
  var MAX_RETRY = 6;
  var everConnected = false;

  function setTransport(state, attempt) {
    if (!transport) return;
    transport.classList.remove("is-unknown", "is-connected", "is-reconnecting", "is-offline");
    transport.classList.add("is-" + state);
    if (transportLabel) {
      transportLabel.textContent =
        state === "connected" ? "Connected" :
        state === "reconnecting" ? "Reconnecting… attempt " + (attempt || 1) :
        state === "offline" ? "Offline" : "Unknown";
    }
    if (reconnectBtn) reconnectBtn.hidden = state !== "offline";
  }
  function setCatchingUp(on) {
    if (catchupTag) catchupTag.hidden = !on;
  }

  function connect() {
    var proto = location.protocol === "https:" ? "wss" : "ws";
    try {
      ws = new WebSocket(proto + "://" + location.host + "/ws");
    } catch (e) { scheduleReconnect(); return; }

    ws.onopen = function () {
      retry = 0;
      setTransport("connected");
      if (everConnected) reconcile();
      everConnected = true;
    };
    ws.onmessage = function (ev) {
      var frame;
      try { frame = JSON.parse(ev.data); } catch (e) { return; }
      if (frame.type === "message") onLiveMessage(frame);
      else if (frame.type === "reaction") onReaction(frame);
      else if (frame.type === "presence") onPresence(frame);
      // unknown frame types are ignored by contract
    };
    ws.onclose = function () { scheduleReconnect(); };
    ws.onerror = function () { if (ws) ws.close(); };
  }

  function scheduleReconnect() {
    if (retry >= MAX_RETRY) {
      setTransport("offline"); // bounded: we stop and wait for the user
      return;
    }
    retry += 1;
    setTransport("reconnecting", retry);
    setTimeout(connect, 500 * Math.pow(2, retry));
  }

  if (reconnectBtn) {
    reconnectBtn.addEventListener("click", function () {
      retry = 0;
      setTransport("unknown");
      connect();
    });
  }

  // Force a socket resubscribe (new room / DM) — membership is a connect-time snapshot.
  function reconnect() {
    retry = 0;
    if (ws) { try { ws.onclose = null; ws.close(); } catch (e) {} ws = null; }
    connect();
  }

  // After a dropout: re-pull room truth + the current window + the open ledger, dedupe by
  // data-msg-id, then drop the Catching up flag.
  function reconcile() {
    setCatchingUp(true);
    var ep = currentEpoch();
    var roomsSnapshotActivity = roomActivityClock;
    fetch("/api/rooms", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (stale(ep) || !data) return false;
        var rooms = data.rooms || [];
        var selectedRoom = null;
        for (var i = 0; i < rooms.length; i++) {
          var ur = rooms[i];
          var room = ur.room || ur;
          if (!room || !room.id) continue;
          if (room.id === selected) {
            selectedRoom = room;
            continue;
          }
          if (ur.unread > 0) {
            bumpUnread(room.id, !!ur.mentioned);
          } else if (currentRoomActivity(room.id) <= roomsSnapshotActivity) {
            clearUnread(room.id);
          }
        }
        if (!selectedRoom) {
          markSelectedUnavailable();
          return false;
        }
        syncSelectedAuthority(selectedRoom);
        return true;
      })
      .catch(function () { return false; })
      .then(function (selectedAvailable) {
        if (stale(ep) || !selectedAvailable) { setCatchingUp(false); return; }
        fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages", { credentials: "same-origin" })
          .then(function (r) { return r.ok ? r.json() : null; })
          .then(function (data) {
            if (!stale(ep) && data) {
              var msgs = (data.messages || []).slice().reverse();
              for (var i = 0; i < msgs.length; i++) {
                var m = msgs[i];
                var existing = rowFor(m.id);
                if (existing) existing.parentNode.replaceChild(buildRow(m), existing);
                else appendRow(buildRow(m));
              }
              markRead(selected, newestRowId());
            }
            if (ledgerOpenTab) loadLedger(ledgerOpenTab, { keepOpen: true });
            setCatchingUp(false);
          })
          .catch(function () { setCatchingUp(false); });
      });
  }

  function onLiveMessage(frame) {
    if (frame.room_id) noteRoomActivity(frame.room_id);
    if (frame.room_id !== selected) {
      if (!frame.deleted && frame.sender_email !== me) {
        bumpUnread(frame.room_id, mentionsMe(frame.body || ""));
        announce("New message in another room");
      }
      return;
    }
    var existing = rowFor(frame.id);
    if (existing) {
      // Edit / soft-delete / the echo of our own 201: replace in place, never duplicate.
      existing.parentNode.replaceChild(buildRow(frame), existing);
      return;
    }
    var wasNear = nearBottom();
    appendRow(buildRow(frame));
    if (wasNear || frame.sender_email === me) {
      scrollTapeBottom(false);
    } else {
      var b = ensureCueTab();
      var n = (b && b.__new ? b.__new : 0) + 1;
      showCueTab(n);
      announce("New message");
    }
    markRead(selected, frame.id);
  }

  function onReaction(frame) {
    if (frame.room_id !== selected) return;
    applyReactions(frame.message_id, frame.reactions);
  }

  function onPresence(frame) {
    if (frame.room_id !== selected) return;
    var who = frame.user_email || "Someone";
    presenceCue(who + (frame.status === "online" ? " connected" : " disconnected"));
  }

  // --- boot sequence -------------------------------------------------------------------------

  // Hydrate the SSR tape: add Edit/Delete to own live rows (the SSR tool cluster carries
  // Reply/React/Pin), align chip aria-pressed with the is-mine marker, and seed the local
  // "mine" sets so the first toggle reconciles against the right baseline.
  (function hydrateRows() {
    var rows = tape.querySelectorAll(".msg[data-msg-id]");
    for (var i = 0; i < rows.length; i++) {
      var row = rows[i];
      var msgId = row.getAttribute("data-msg-id");
      var lifecycle = row.getAttribute("data-lifecycle");
      var own = row.getAttribute("data-own") === "true";
      var head = row.querySelector(".msg__head");
      var tools = head ? head.querySelector(".msg__tools") : null;
      if (head && !tools) {
        head.appendChild(buildTools(row, { deleted: lifecycle !== "live" }, own));
      } else if (tools && own && lifecycle === "live" && selectedAuthority === "active") {
        if (!tools.querySelector('[data-act="edit"]')) tools.appendChild(makeTool("edit", "Edit"));
        if (!tools.querySelector('[data-act="delete"]')) tools.appendChild(makeTool("delete", "Delete"));
      }
      var chips = row.querySelectorAll(".reaction");
      for (var j = 0; j < chips.length; j++) {
        var isMineChip = chips[j].classList.contains("is-mine");
        chips[j].setAttribute("aria-pressed", isMineChip ? "true" : "false");
        if (!chips[j].getAttribute("aria-label")) {
          chips[j].setAttribute("aria-label", "React " + chips[j].getAttribute("data-emoji"));
        }
        if (isMineChip) mineSet(msgId).push(chips[j].getAttribute("data-emoji"));
      }
    }
  })();

  // An SSR-opened ledger (?ledger=…) becomes the tracked open tab so reconciles refresh it.
  var ssrTab = document.querySelector(".ledger__tab[aria-current]");
  if (ssrTab && ssrTab.id.indexOf("pb-tab-") === 0) ledgerOpenTab = ssrTab.id.slice(7);

  deck.classList.toggle("deck--archived", selectedAuthority === "archived");
  setComposerEnabled(selectedAuthority === "active" && !!selected);
  autosize();
  updateBudget();
  scrollTapeBottom(true);
  setTransport("unknown");
  connect();

  // SSR reply context (?reply_to=) arrives pre-rendered in the loop line; SSR receipt
  // (?receipt_message=) arrives in the strip. Both are already the server-confirmed truth.
  var ssrLocated = tape.querySelector(".is-located");
  if (ssrLocated) {
    ssrLocated.setAttribute("tabindex", "-1");
    ssrLocated.focus({ preventScroll: true });
  } else {
    var fragment = location.hash.match(/^#msg-(.+)$/);
    if (fragment && rowFor(fragment[1])) locateRow(fragment[1]);
  }
  markRead(selected, newestRowId());
})();
