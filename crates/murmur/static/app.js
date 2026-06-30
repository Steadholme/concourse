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

  var $ = function (sel) { return document.querySelector(sel); };
  var timeline = $("#timeline");
  var roomList = $("#room-list");
  var roomTitle = $("#room-title");
  var presence = $("#presence");
  var composer = $("#composer");
  var input = $("#composer-input");

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

    var head = document.createElement("div");
    head.className = "msg__head";
    var author = document.createElement("span");
    author.className = "msg__author";
    author.textContent = m.sender_email || m.sender_sub || "—";
    var time = document.createElement("span");
    time.className = "msg__time";
    time.textContent = fmtTime(m.created_at);
    head.appendChild(author);
    head.appendChild(time);

    var body = document.createElement("div");
    body.className = "msg__body";
    appendLinkified(body, m.body || "");

    row.appendChild(head);
    row.appendChild(body);
    return row;
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
      var a = document.createElement("a");
      a.href = trimmed;
      a.textContent = trimmed;
      a.rel = "noopener noreferrer nofollow";
      a.target = "_blank";
      el.appendChild(a);
      if (trimmed.length < raw.length) {
        el.appendChild(document.createTextNode(raw.slice(trimmed.length)));
      }
      last = match.index + raw.length;
    }
    if (last < line.length) el.appendChild(document.createTextNode(line.slice(last)));
  }

  function scrollToBottom() {
    if (timeline) timeline.scrollTop = timeline.scrollHeight;
  }

  function clearTimeline() { if (timeline) timeline.innerHTML = ""; }

  // --- room switching + history --------------------------------------------

  function selectRoom(id, name) {
    selected = id;
    if (roomTitle) roomTitle.textContent = name || id;
    if (input) input.placeholder = "Message " + (name || id) + "…";
    var items = roomList ? roomList.querySelectorAll(".room") : [];
    for (var i = 0; i < items.length; i++) {
      items[i].classList.toggle("is-active", items[i].getAttribute("data-room-id") === id);
    }
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

  function sendMessage() {
    var body = input ? input.value.trim() : "";
    if (!body) return;
    fetch("/api/rooms/" + encodeURIComponent(selected) + "/messages", {
      method: "POST",
      headers: apiHeaders(true),
      credentials: "same-origin",
      body: JSON.stringify({ body: body })
    })
      .then(function (r) {
        if (r.ok && input) { input.value = ""; input.style.height = "auto"; }
        // The message arrives back over /ws and is appended there (no optimistic dup).
      })
      .catch(function () {});
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
    var btn = document.createElement("button");
    btn.className = "room__btn";
    btn.type = "button";
    btn.textContent = room.name;
    li.appendChild(btn);
    roomList.appendChild(li);
  }

  function cssEscape(s) { return String(s).replace(/["\\]/g, "\\$&"); }

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
    };
    ws.onclose = function () { if (presence) presence.textContent = ""; scheduleReconnect(); };
    ws.onerror = function () { if (ws) ws.close(); };
  }

  function scheduleReconnect() {
    retry = Math.min(retry + 1, 6);
    setTimeout(connect, 500 * Math.pow(2, retry));
  }

  function onLiveMessage(frame) {
    if (frame.room_id !== selected) return; // (unread bumps for other rooms: future work)
    if (timeline && timeline.querySelector('[data-id="' + cssEscape(frame.id) + '"]')) return;
    var emptyEl = timeline ? timeline.querySelector(".timeline__empty") : null;
    if (emptyEl) emptyEl.remove();
    var nearBottom = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 80;
    timeline.appendChild(buildMessage(frame));
    if (nearBottom) scrollToBottom();
    markRead(selected);
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
  if (selected) markRead(selected);
  connect();
})();
