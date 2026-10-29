//! `ib-imessage` — the iMessage transport for iphonebridge.
//!
//! iphonebridge's other transports read the phone over Bluetooth, which caps
//! what's possible: MAP delivers incoming text and nothing else — no messages
//! you sent, no attachments, no real tapback targets, no reply threading, and
//! no way to send anything richer than plain text. This helper removes that
//! ceiling by speaking to Apple directly, as the Apple ID itself, using the
//! registration OpenBubbles established.
//!
//! It exists as a separate process rather than a Python extension because
//! rustpush is the only workable iMessage implementation and it is Rust and
//! deeply async. Talking to it over a socket keeps the daemon free of a
//! native-extension build step, and keeps a panic in Apple-protocol parsing
//! from taking the UI down with it.
//!
//! ## Protocol
//!
//! Line-delimited JSON both ways, on a unix socket (`--socket PATH`) or on
//! `127.0.0.1:PORT` (`--tcp PORT`, the only option on Windows). One line is
//! one UTF-8 JSON object terminated by `\n`. Every connected client receives
//! every event; replies go only to the client that sent the request.
//!
//! ### Envelope
//!
//! Request:  `{"id":"<string>","cmd":"<snake_case command>", ...fields}`
//! `id` is optional but must be a JSON *string* to be echoed; any other type
//! is treated as absent (`"id":null` on the reply). Unknown fields are
//! ignored. An unparseable line still gets a reply carrying its `id`.
//!
//! Success:  `{"id":"1","ok":true, ...command-specific fields}`
//! Failure:  `{"id":"1","ok":false,"error":"<message with cause chain>"}`
//! A malformed request fails with `"error":"bad request: <serde message>"`.
//! There is exactly one reply per request line. Requests on one connection
//! are answered in order, one at a time; a slow upload blocks the next
//! request on that connection (open a second connection to avoid that).
//!
//! ### ChatRef — the `chat` field
//!
//!     {"participants":["tel:+12155550100","mailto:a@b.com"],
//!      "name":"Optional group name", "guid":"Optional chat guid",
//!      "after_guid":"Optional newest-known message guid"}
//!
//! `participants`: required, full handles (`tel:+E164` or `mailto:`). Our
//! own handle is added by rustpush if missing. More than two participants
//! including us makes it a group. `name` -> `cv_name` (group display name).
//! `guid` -> `sender_guid`, the conversation id iMessage threads a group by.
//!
//! **New vs existing group.** Omit `guid` and rustpush mints a fresh one in
//! `MessageInst::prepare_send`; that is how a group is created — there is no
//! separate "create group" call. The minted id comes back as `chat_guid` on
//! the reply; store it. To post into an *existing* group, pass that value, or
//! `inst.conversation.sender_guid` from any incoming event in that group, as
//! `chat.guid`, together with the current participant list and name.
//! Whether iOS merges a guid-less send into an existing thread with the same
//! participants is device behaviour, not something this helper controls —
//! pass the guid. [VERIFY on a device]
//!
//! ### Commands
//!
//! `status` -> `{"handles":[...],"send_as":"tel:+1…","warnings":[...],"note":"…",
//!   "profile":{"found":bool,"display_name":"…"|null,"record_key":"…"|null,
//!              "source":"<file it was imported from>"|null,
//!              "share":"always|first|never"}}`
//! `display_name` is OpenBubbles' local name field captured at import time,
//! not a live read of the CloudKit record recipients see.
//!
//! **Adopting the profile from our own device.** When Name & Photo Sharing
//! is set on the account's Mac or iPhone, that device sends `UpdateProfile`
//! (or `ShareProfile`) to the account's other devices. When such a message
//! arrives from one of our registered handles, the helper writes its record
//! id + key to `share_profile.json` (atomically), loads it live, and logs
//! `adopted shared profile from own device`. `status.profile.source` reads
//! `"own device"` and `display_name` is `--profile-name` (default "Group G"),
//! since the update carries no name. A profile update from anyone else is
//! never adopted, only emitted as the usual `message` event. An update with
//! no profile from our own device (profile deleted) drops ours.
//!
//! **Share Name and Photo.** With `--share-profile always|first` and a
//! `share_profile.json` in the state dir (see `profile.rs`), `send` sets
//! `NormalMessage.embedded_profile`, so recipients get the name and photo.
//! `first` means once per conversation (chat guid, else participant set)
//! per helper run. Default `never`. Tapbacks, edits and other commands never
//! carry it.
//!
//! `handles` -> `{"handles":["mailto:…","tel:+1…"]}`
//!
//! `send` -> `{"guid":"<MESSAGE GUID, uppercase>","chat_guid":"<conversation guid>"}`
//!
//!     {"id":"7","cmd":"send","chat":{...},
//!      "text":"Hello Gerry, plan attached",      // optional if attachments
//!      "subject":"Optional subject line",
//!      "reply_guid":"<guid>", "reply_part":"0",   // reply threading
//!      "effect":"com.apple.MobileSMS.expressivesend.impact",
//!      "format":[{"start":0,"len":5,"bold":true},
//!                {"start":6,"len":5,"italic":true,"underline":true},
//!                {"start":12,"len":4,"effect":"shake"}],
//!      "mentions":[{"start":6,"len":5,"handle":"tel:+12155550100"}],
//!      "attachments":[{"path":"C:\\abs\\plan.pdf"},
//!                     {"path":"C:\\abs\\photo.heic","name":"site.heic",
//!                      "mime":"image/heic","uti":"public.heic"}],
//!      "as_audio_message":false}
//!
//! - `text`: default `""`. A send needs non-empty `text`, at least one
//!   attachment, or both.
//! - `format` / `mentions`: **`start` and `len` are UTF-16 code units** (as
//!   NSString and JavaScript count), not bytes or code points. An emoji
//!   outside the BMP is 2 units. Ranges must lie within `text`, have `len >
//!   0`, and not split a surrogate pair. The text is split at every range edge
//!   into `MessagePart::Text` runs; flags from overlapping ranges combine.
//!   Format flags: `bold`, `italic`, `underline`, `strikethrough` (bools,
//!   default false). `effect`: one of `big small shake nod explode ripple
//!   bloom jitter` (iOS 18 animated text). rustpush's `TextFormat` is either
//!   flags *or* one effect per run, so a span with both, or with two effects,
//!   is rejected. Mentions become `MessagePart::Mention(handle, span text)`;
//!   `handle` must start `tel:` or `mailto:`; mentions may not overlap each
//!   other or any format range. `format`/`mentions` without `text` is an error.
//! - `attachments`: absolute local paths. `name` defaults to the file name;
//!   `mime` and `uti` each default from the extension (images incl. HEIC,
//!   PDF, video, audio, common office types; DWG/DXF and anything unknown go
//!   as `application/octet-stream` / `public.data`). Every path is checked
//!   before any upload starts. Each file is encrypted and uploaded to MMCS
//!   (`MMCSFile::prepare_put` + `MMCSFile::new`, the two halves of
//!   `Attachment::new_mmcs`). Parts are ordered text first, then attachments
//!   in the order given. A failed upload fails the whole send with
//!   `ok:false`, e.g. `"uploading attachment 1 (C:\\x.jpg): MMCS upload: …"`,
//!   and nothing is sent.
//! - `as_audio_message`: sets rustpush's `NormalMessage.voice`, which writes
//!   `voice-audio`/`voice-e` so iOS shows an audio-message bubble. Requires
//!   exactly one attachment and no text; send a `.caf` (iOS voice memos are
//!   Opus or AAC in CAF). The helper does not transcode.
//! - `effect`: a bubble/screen effect id, passed through untouched.
//!
//! `set_profile` -> `{"found":true,"display_name":"Group G","record_key":"<id>",
//!   "source":"<STATE_DIR>\\share_profile.json","share":"always|first|never"}`
//!
//!     {"id":"10","cmd":"set_profile","name":"Group G","first":"Group",
//!      "last":"G","photo":"C:\\abs\\pic.jpg"}
//!     {"id":"11","cmd":"set_profile","name":"Group G","first":"Group",
//!      "last":"G","photo":"C:\\abs\\pic.jpg","tfa_code":"123456"}
//!
//! Creates our Share Name and Photo record in CloudKit (rustpush
//! `ProfilesClient::set_record`), replacing any record the helper already
//! holds, writes `share_profile.json`, and makes it live at once: the next
//! `send` under `--share-profile always|first` embeds it, and `status`
//! reports it. No restart.
//! - `name` (required): the display name recipients see. `first`/`last`
//!   default to `""`. `photo` optional: JPEG or PNG, center-cropped to a
//!   square and re-encoded as a 512x512 JPEG. HEIC is rejected.
//! - Signs in to the Apple ID with `STATE_DIR/gsa.plist` (copied from
//!   OpenBubbles by `--import-from`). If Apple wants 2FA the reply is
//!   `ok:false` with `error` starting **`2fa_required:`**. A code is pushed to
//!   the account's trusted devices. Repeat the call with the same fields plus
//!   `tfa_code`. The pending sign-in lives in memory only. After a
//!   successful sign-in the client is reused, so later calls need no 2FA
//!   until the helper restarts.
//! - May take tens of seconds (GSA sign-in, MMCS/CloudKit upload). Use a
//!   separate connection if other requests must not wait behind it.
//!
//! `set_icon` -> `{"guid":…,"chat_guid":…}` — set or clear a group photo.
//!
//!     {"id":"8","cmd":"set_icon","chat":{...,"guid":"<chat guid>"},
//!      "path":"C:\\abs\\photo.png","group_version":0}
//!     {"id":"9","cmd":"set_icon","chat":{...,"guid":"<chat guid>"}}
//!
//! With `path` the file is uploaded to MMCS and sent as `IconChange`. Without
//! `path` an `IconChange` with no file is sent, which removes the photo.
//! rustpush notes the image "should be 570x570 png"; the helper does not
//! resize or convert. `group_version` defaults to 0, as on
//! `set_participants`. [VERIFY what value iOS expects]
//!
//! `download_attachment` -> `{"path":"<dest>","bytes":<n>}`
//!
//!     {"id":"3","cmd":"download_attachment","guid":"<msg guid>","index":0,
//!      "dest":"C:\\abs\\out.jpg"}
//!
//! `index` counts attachment parts within that message. Only messages
//! received since the helper started (newest 256 with attachments) can be
//! fetched.
//!
//! `react` -> `{"guid","chat_guid"}`
//!
//!     {"cmd":"react","chat":{...},"target_guid":"<guid>","target_text":"…",
//!      "target_part":0,"kind":"heart|like|dislike|laugh|emphasize|question",
//!      "emoji":"🔥","enable":true}
//!
//! `emoji` wins over `kind`. `enable:false` removes the tapback.
//!
//! `edit` -> `{"guid","chat_guid"}`:
//! `{"cmd":"edit","chat":{...},"target_guid":"<guid>","part":0,"text":"new","format":[...]}`
//!
//! `unsend` -> `{"guid","chat_guid"}`:
//! `{"cmd":"unsend","chat":{...},"target_guid":"<guid>","part":0}`
//!
//! `typing` -> `{"guid","chat_guid"}`: `{"cmd":"typing","chat":{...},"typing":true}`
//!
//! `mark_read` -> `{"guid","chat_guid"}`:
//! `{"cmd":"mark_read","chat":{...},"guid":"<newest message guid read>"}`
//!
//! `read_receipt` -> `{"guid","chat_guid"}`:
//! `{"cmd":"read_receipt","chat":{...},"guid":"<message guid>"}`
//!
//! Both send IDS command 102 (`Message::Read`) to every participant,
//! including our other devices. The sender sees "Read" only when the
//! receipt's id is the guid of their message (and only if they have read
//! receipts on for us — their device decides display). `mark_read` without
//! `guid` sends a random id: it reaches the sender but matches no message.
//!
//! **Delivery receipts** are sent automatically for incoming messages from
//! others (see `Session::acknowledge`); `--no-delivery-receipts` disables
//! that. Nothing is emitted for them.
//!
//! `rename` -> `{"guid","chat_guid"}`: `{"cmd":"rename","chat":{...},"name":"New"}`
//!
//! `send_poll` -> `{"guid","chat_guid","option_ids":["<UUID>",…],"session":"<UUID>"}`
//!
//!     {"cmd":"send_poll","chat":{...},"question":"Site visit day?",
//!      "options":["Tue","Wed","Thu"]}
//!
//! At least two non-empty options. `question` (default `""`) goes in the
//! payload's `item.title`, as the sbaresearch fork sends it; no separate
//! caption message is sent. `option_ids` are in `options` order. The poll is
//! registered so later `poll_vote` events on it carry option text. Wire
//! shape: a Polls extension balloon (`requiredCapabilities` =
//! `["supports-polls"]`, `sendAsText`, the Polls icon, breadcrumb and xml
//! "Sent a poll"). [VERIFY on a device: ported, not yet sent live]
//!
//! `poll_vote` -> `{"guid","chat_guid"}`
//!
//!     {"cmd":"poll_vote","chat":{...},"poll_guid":"<guid>",
//!      "option_ids":["<UUID>",…],"session":"<UUID, optional>"}
//!
//! Sets our *whole* selection; `[]` withdraws. Sent as a `React` with
//! `Extension { is_meta: true }` (associated type 4000) on `poll_guid`.
//! `session` is the poll's balloon session; omit it for a poll sent or
//! received since the helper started. Ids not on a known poll are rejected.
//! [VERIFY on a device]
//!
//! **Update guids are aliases of the poll.** Once an option is added, iOS
//! aims later votes at the newest update message, not at the poll (seen
//! live 2026-10-28). The helper maps every update guid to its original poll
//! guid: on our own `poll_add_option`, and on any received type-2 update
//! carrying `orderedPollOptions`, chaining through earlier updates. Events
//! always report the original guid (`poll.guid`, `poll_vote.poll_guid`), and
//! `poll_vote` / `poll_add_option` accept either guid. `poll_vote` sends to
//! the poll's newest message, as the iPhone did, and its reply adds
//! `poll_guid` (original) and `target_guid` (where the vote went).
//! `poll_add_option` stays associated to the original poll, as captured.
//! A received update aliases even when its poll is unknown here. Aliases
//! are in memory only.
//!
//! `poll_add_option` -> `{"guid","chat_guid","option_id":"<UUID>","poll_guid"}`
//!
//!     {"cmd":"poll_add_option","chat":{...},"poll_guid":"<guid>",
//!      "text":"Fri","session":"<UUID, optional>"}
//!
//! Appends one option with a new UUID and our handle (bare) as its
//! `creatorHandle`. The base is the newest full poll payload seen or sent
//! since the helper started; an unknown poll is refused. `session` defaults
//! to the remembered one. Empty text, or text equal to an existing option,
//! is refused (the iPhone itself allowed two options both named "test").
//! The option map updates on success, so votes on the new option resolve.
//! Envelope, from an iPhone capture (2026-10-28,
//! poll-addoption-capture.log): a `React` on the poll guid with
//! `Extension { is_meta: false }` (associated type 2), `to_part` none, a
//! body of one `Object("Sent a poll")` part, and the creation's balloon
//! repeated: `MSMessageTemplateLayout` captioned "Sent a poll", the
//! `["supports-polls"]` capabilities, `sendAsText`, `ld_text` "Sent a poll",
//! live, same session, with the URL carrying the full extended
//! `orderedPollOptions` and `?src=p&c=<count>`. Two differences from the
//! capture: our icon is the fork's 1462-byte JPEG where the iPhone's was
//! 2820 bytes (only its size was logged), and `to_text` is empty (not in the
//! capture). [VERIFY on a device that iOS shows the added option]
//!
//! `set_participants` -> `{"guid","chat_guid"}`:
//! `{"cmd":"set_participants","chat":{...},"participants":[...],"group_version":0}`
//!
//! `send_link` -> `{"guid","chat_guid","card":bool,"card_note"|"card_error":"…"|null}`
//!
//!     {"cmd":"send_link","chat":{...},"url":"https://atlas.phila.gov/…",
//!      "title":"Optional","summary":"Optional","site_name":"Optional",
//!      "image_path":"C:\\abs\\card.jpg","fetch":true}
//!
//! Sends a rich link card: rustpush's `NormalMessage.link_meta`, which goes
//! out as a `com.apple.messages.URLBalloonProvider` balloon carrying an
//! `LPLinkMetadata`, with the image inline in the balloon (no MMCS). The text
//! body is the URL. `url` must be absolute http(s). `image_path`: JPEG or PNG.
//! With `fetch` (default true), a missing title or image is filled from the
//! page's OpenGraph tags (`og:title`, `og:description`, `og:site_name`,
//! `og:image`; fallback `<title>` and `description`): 5 s timeout per
//! request, 512 KB of HTML, 2 MB of image. Given fields always win.
//! **Fallback:** with no title and no image, or if the card send fails, the
//! URL goes as plain text and `card` is false, with the reason in
//! `card_error`. A card that failed after reaching some devices can arrive
//! next to the plain one. `site_name` is the `siteName` key, added to the
//! vendored struct. [VERIFY on a device that iOS renders our card]
//!
//! `download_link_image` -> `{"path","bytes","mime"}`:
//! `{"cmd":"download_link_image","guid":"<link msg guid>","dest":"C:\\abs\\o.jpg"}`.
//! The newest 64 received link images are cached.
//!
//! `focus_status` -> `{"statuses":[{"handle","shared":bool,"subscribed":bool,
//!   "known":bool,"focused":bool|null,"mode":"…"|null,"allowed":bool|null,
//!   "at":"<RFC 3339>"|null,"at_epoch":…,"observed_at":…}],"sharers":[…]}`
//!
//!     {"cmd":"focus_status","handles":["tel:+12155550100"]}   // [] = every sharer
//!
//! Needs `--focus`; otherwise `ok:false` with the reason (also in
//! `status.focus`). Read-only: the helper never publishes a Focus status.
//! A handle has a status only if that person's device shared its Focus
//! key with us (`shared`). We cannot ask for it. Every sharer is subscribed
//! automatically. `known` turns true when their channel delivers a status.
//! `focused` is the opposite of StatusKit's `active` flag. `mode` is the
//! Focus mode id. `allowed` means our messages would still notify them.
//! `at` is when their device created the status: the "since when".
//! Statuses are in memory only and are re-delivered after a restart.
//! See `focus.rs`.
//!
//! **`--focus` and the registration.** Receiving needs the IDS service
//! `com.apple.private.alloy.multiplex1`. The registration includes it, but
//! rustpush re-registers any service whose stored `data_hash` differs from
//! its own, and this build cannot re-register. `--focus` is refused at
//! startup unless the hashes match. On the firm's registration as of
//! 2026-10-28 they do not (stored 13329886844215203963, this build
//! 10279869897339162389), so `--focus` is refused there today.
//!
//! `set_background` -> `{"guid","chat_guid","version":n,"cleared":bool,"poster":"photo|…"|null}`
//!
//!     {"cmd":"set_background","chat":{...,"guid":"<chat guid>"},"image_path":"C:\\abs\\site.jpg"}
//!     {"cmd":"set_background","chat":{...},"gradient":{"colors":["#DCD2F2",[0.23,0.69,0.93]]}}
//!     {"cmd":"set_background","chat":{...},"dynamic":"ocean_2"}
//!     {"cmd":"set_background","chat":{...},"path":"C:\\abs\\bg.zip"}
//!     {"cmd":"set_background","chat":{...,"guid":"<chat guid>"},"version":4}   // clear
//!
//! IDS command 138, `Message::SetTranscriptBackground`. At most one source;
//! none clears. The payload is uploaded to MMCS, as `set_icon` does.
//! iOS's payload is a zip: `watchBackground` (a plist with `isHighKey`,
//! `luminance`, an 820x1003 PNG preview, `extensionIdentifier`) and
//! `poster` (a PosterKit archive). `src/bgbuild.rs` builds it from
//! templates captured from an iPhone (`assets/bg/`):
//! - `image_path`: any JPEG, PNG or HEIC. It is cropped to fill the
//!   template's 1292x2796 portrait layer and encoded to HEIC by
//!   `assets/bg/prep_photo.py` (Pillow + pillow-heif; `GROUPG_PYTHON`
//!   picks the interpreter). The watch preview, `luminance`, `isHighKey`
//!   and `Contents.json` `clockAreaLuminance` are recomputed from it. All
//!   frames are kept, since they refer to the layer's size. Every UUID is
//!   fresh (configuration, poster identifier, suggestion, and the asset id
//!   that names the contents folder).
//! - `gradient.colors`: exactly two, `"#RRGGBB[AA]"` or `[r,g,b(,a)]` in
//!   0..1, written as `userInfo.custom` `r/g/b/a//r/g/b/a`.
//! - `dynamic`: `aurora_2`, `clouds_2` or `ocean_2`, the three presets
//!   captured (`userInfo.identifier`), each with Apple's own preview.
//! - `path`: a ready payload, e.g. one saved with `download_background`.
//!   Images are refused here; use `image_path`.
//!
//! `cid` is `chat.guid`. `version` (`tbid`) defaults to one past the newest
//! seen or sent for that chat, else 1. [VERIFY on a device: that iOS shows
//! our builds; the derived luminances and the watch crop are best fits, not
//! Apple's formulas; the gradient preview direction]
//!
//! `download_background` -> `{"path","bytes","preview":"<path>"|null,"poster":"…"|null}`:
//! `{"cmd":"download_background","guid":"<background msg guid>","dest":"C:\\abs\\bg.zip"}`.
//! Writes the payload, plus its watch preview image beside it as
//! `<dest>.preview.<ext>`.
//!
//! ### Events (no `id` unless stated)
//!
//! `{"event":"message","inst":<MessageInst>}` — everything rustpush parses
//! off APNs, including copies of our own messages sent from other devices on
//! the account. `inst` is rustpush's `MessageInst` serialised by serde, as is:
//!
//!     {"id":"<GUID>","sender":"tel:+1…"|null,
//!      "conversation":{"participants":[...],"cv_name":…|null,
//!                      "sender_guid":"<chat guid>"|null,"after_guid":…}|null,
//!      "message":<Message>,"sent_timestamp":<ms since epoch>,
//!      "target":…,"send_delivered":bool,"verification_failed":bool,
//!      "certified_context":…}
//!
//! The group guid is `inst.conversation.sender_guid`; the group name is
//! `inst.conversation.cv_name`. `message` is serde's externally tagged enum —
//! a unit variant is a bare string, any other is a one-key object. Tell the
//! kinds apart by that tag:
//!
//! | `inst.message`                                   | Meaning |
//! | ---                                              | --- |
//! | `{"Message":{"parts":[…],"effect":…,"reply_guid":…,"subject":…,"voice":bool,…}}` | a message |
//! | `"Delivered"`                                    | delivery receipt (IDS command 101) |
//! | `"Read"`                                         | read receipt (IDS command 102) |
//! | `{"Typing":[true,null]}` / `{"Typing":[false,null]}` | typing started / stopped; 2nd item is a third-party app or null |
//! | `{"React":{"to_uuid":…,"to_part":…,"reaction":…,"to_text":…}}` | tapback |
//! | `{"Edit":{…}}` / `{"Unsend":{…}}`                 | edit / unsend |
//! | `{"RenameMessage":{"new_name":…}}`               | group renamed |
//! | `{"ChangeParticipants":{"new_participants":[…],"group_version":n}}` | members changed |
//! | `{"IconChange":{"file":{…}|null,"group_version":n}}` | group photo set / removed |
//! | `"MessageReadOnDevice"`, `"PeerCacheInvalidate"`, `{"Error":{…}}`, … | other rustpush variants, passed through |
//!
//! For `Delivered` and `Read`, `inst.id` is the IDS message uuid of the
//! receipt, which by IDS convention is the guid of the message acknowledged,
//! `inst.sender` is who delivered/read it, and `inst.conversation` is null.
//! [VERIFY the id mapping against a live receipt] A typing indicator that
//! arrives unencrypted (IDS `is_typing: 0`) carries a two-person conversation
//! built from sender and target and is always `Typing(true)`; the encrypted
//! form carries the real conversation and either state.
//!
//! Parts inside `{"Message":…}`, under `parts[i].part`:
//! `{"Text":["run",{"Flags":{"bold":…,"italic":…,"underline":…,"strikethrough":…}}]}`,
//! `{"Text":["run",{"Effect":"Shake"}]}`, `{"Mention":["tel:+1…","Gerry"]}`,
//! `{"Attachment":{"a_type":{"MMCS":[…]},"part":n,"uti_type":…,"mime":…,"name":…,"iris":bool}}`,
//! `{"Object":"breadcrumb"}`. Byte fields (MMCS keys) serialise as number
//! arrays and cannot be sent back; use `download_attachment`.
//!
//! `{"event":"upload_progress","id":"<request id>"|null,"index":0,"count":2,
//!   "bytes":1048576,"total":5242880}` — during `send` attachments and
//! `set_icon`, roughly every 10% per file. `id` is the request it belongs to.
//! `total` is MMCS's count and includes encryption overhead, so it can differ
//! slightly from the file size. Advisory only: the reply is the result.
//!
//! **Native polls (iOS 26).** A poll arrives as an ordinary
//! `message` event whose `Message.app.bundle_id` ends in
//! `com.apple.messages.Polls`; a vote arrives as a `React` with an
//! `Extension` reaction (associated type 4000) whose `to_uuid` is the poll.
//! The raw `message` event is always sent; these follow it:
//!
//!     {"event":"poll","guid":"<poll message guid>","sender":"<handle>"|null,"chat":<ConversationData>|null,
//!      "question":"…","creator":"<handle>"|null,
//!      "options":["…",…],"option_ids":["<UUID>",…]}
//!     {"event":"poll_vote","poll_guid":"<guid>","sender":"<handle>"|null,"chat":…,"voter":"<handle>"|null,
//!      "option":"<text, or the option id if the poll is unknown>",
//!      "option_id":"<UUID>","option_known":bool,
//!      "selected":[{"option_id":"<UUID>","option":"…"|null},…]}
//!
//! - `question` is often `""`: iOS can show the question as a separate text
//!   message replying to the poll (per openclaw/imsg). Read the next message
//!   with `reply_guid` = the poll guid if it is empty.
//! - A vote payload is the voter's whole current selection. One `poll_vote`
//!   is emitted per selected option, each carrying the full `selected` list;
//!   consumers should *replace* that voter's selection with `selected`. An
//!   empty selection (vote withdrawn) emits one `poll_vote` with
//!   `option`/`option_id` null and `selected: []`. [VERIFY on a device]
//! - Option texts are remembered only for polls seen since the helper
//!   started; votes on older polls carry `option_known: false`.
//! - `sender` (on both events) is `inst.sender`: the handle Apple delivered
//!   and verified (`tel:+1…` / `mailto:…`), or null if the envelope had none.
//!   It is never read from the payload. `creator` and `voter` come from the
//!   payload (`creatorHandle`, `participantHandle`) and **any chat member can
//!   forge them**; authorise on `sender`, not on those.
//! - Handles inside the poll payload (`creator`, `voter`) are as iOS writes
//!   them (`+1…` or an email), without the `tel:`/`mailto:` prefix.
//! - A type-2 update that carries the option list (an option was added) is
//!   re-emitted as `poll` with the original guid.
//! - rustpush does not serialise `ExtensionApp.balloon`, so the poll URL is
//!   *not* in `inst`. Every extension balloon is logged at info level as
//!   `extension balloon on <guid> … bid=… url=…` for capture and debugging.
//! - The balloon session id of every poll seen is remembered, for `poll_vote`.
//! Format and sources: `src/poll.rs`.
//!
//! **Rich links.** After the raw `message` event, a message with
//! `Message.link_meta` emits:
//!
//!     {"event":"link","guid","sender":"<handle>"|null,"chat":<ConversationData>|null,
//!      "url","original_url","title","summary","site_name",
//!      "image":{"mime","index","bytes","available":bool,"size":"{w, h}"|null,
//!               "source_url"|null}|null}
//!
//! `sender` is `inst.sender`, Apple-verified. Everything else is the
//! sender's own claim about the page. Fetch the image with
//! `download_link_image`.
//!
//! **Focus** (`--focus` only):
//! `{"event":"focus","handle","focused":bool,"mode","allowed","at","at_epoch","observed_at"}`
//! on each status a sharer publishes, and `{"event":"focus_shared","handle"}`
//! when someone first shares their Focus key with us. `handle` is the IDS
//! sender of the key-sharing message, which Apple delivers and verifies.
//!
//! **Backgrounds:** `{"event":"background","guid","chat":"<cid>"|null,
//! "sender","cleared":bool,"version":n,"background_id"?,"payload_version"?,
//! "bytes"?}`. rustpush delivers this with no conversation, so `inst.
//! conversation` is null and `chat` is the payload's `cid`, which is
//! assumed but not verified to be the chat guid. `sender` is `inst.sender`.
//! `version` is `tbid`, the background's sequence number.
//!
//! `{"event":"parse_error","error":"<rustpush error>"}` — an APNs message that
//! rustpush could not parse.
//!
//! `{"event":"disconnected"}` — the APNs connection closed; the helper exits
//! about 200 ms later and must be restarted by its supervisor.
//!
//! Events embed rustpush's `MessageInst` verbatim under `inst`. That is a
//! deliberate choice: the Python side already has to understand iMessage
//! semantics, and re-projecting every message type here would silently drop
//! whatever rustpush learns to parse next.
//!
//! ## One connection at a time
//!
//! APNs permits a single live connection per push token, and this helper uses
//! the *same* token as OpenBubbles. Running both at once makes them displace
//! each other in a loop. The daemon is responsible for not doing that; see
//! `SINGLETON_NOTE` below.

mod compose;
mod background;
mod bgbuild;
mod focus;
mod link;
mod migrate;
mod poll;
mod profile;
mod setprofile;
mod state;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};

use rustpush::{
    APSConnectionResource, ConversationData, EditMessage, IMClient, Message, MessageInst,
    IndexedMessagePart, MessagePart, MessageParts, MessageType, NormalMessage, ReactMessage,
    ReactMessageType, Reaction, RenameMessage, UnsendMessage, MADRID_SERVICE,
};

use state::{registration_age, StateDir};

/// Why the daemon must serialise access: see the module docs. Kept as a
/// constant so the reason travels with the code that would break.
const SINGLETON_NOTE: &str =
    "APNs allows one connection per push token; OpenBubbles must be closed \
     while this helper is running, or the two will fight over the token.";

/// Warn once a registration is this old. Apple's IDS certs run about a month;
/// warning at three weeks leaves time to re-register via OpenBubbles before
/// sending starts failing.
const REGISTRATION_WARN_SECS: u64 = 21 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

/// A conversation, as the Python side describes it.
///
/// `participants` is the identity of a chat in iMessage — there is no server
/// side chat id to quote back. For a group, `guid` pins the specific thread
/// so a rename or a participant change lands on the right one.
#[derive(Deserialize, Clone)]
struct ChatRef {
    participants: Vec<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    guid: Option<String>,
    /// Newest known message in the thread. iMessage uses it to order
    /// messages; omitting it puts the message at the end, which is usually
    /// what we want.
    #[serde(default)]
    after_guid: Option<String>,
}

impl From<ChatRef> for ConversationData {
    fn from(c: ChatRef) -> Self {
        ConversationData {
            participants: c.participants,
            cv_name: c.name,
            sender_guid: c.guid,
            after_guid: c.after_guid,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    /// Liveness plus registration health, so the UI can show why sending is
    /// about to break rather than only that it broke.
    Status,
    /// Our own registered handles — the Python side needs them to tell an
    /// outgoing message from an incoming one.
    Handles,
    Send {
        chat: ChatRef,
        /// May be empty when `attachments` is not.
        #[serde(default)]
        text: String,
        #[serde(default)]
        subject: Option<String>,
        /// Guid of the message being replied to, for real reply threading.
        #[serde(default)]
        reply_guid: Option<String>,
        #[serde(default)]
        reply_part: Option<String>,
        /// e.g. "invisibleink", "slam" — an iMessage screen/bubble effect.
        #[serde(default)]
        effect: Option<String>,
        /// Formatting ranges over `text`, in UTF-16 code units.
        #[serde(default)]
        format: Vec<compose::FormatRange>,
        /// Mentions over `text`, in UTF-16 code units.
        #[serde(default)]
        mentions: Vec<compose::MentionRange>,
        /// Local files, uploaded to MMCS and sent after the text.
        #[serde(default)]
        attachments: Vec<AttachmentSpec>,
        /// Mark the message as a voice memo. Needs exactly one attachment
        /// and no text.
        #[serde(default)]
        as_audio_message: bool,
    },
    /// Create or replace our Share Name and Photo record; see setprofile.rs.
    SetProfile {
        /// Display name recipients see.
        name: String,
        #[serde(default)]
        first: String,
        #[serde(default)]
        last: String,
        #[serde(default)]
        photo: Option<String>,
        /// Second call only, when the first answered `2fa_required:`.
        #[serde(default)]
        tfa_code: Option<String>,
    },
    /// Set (`path`) or clear (no `path`) a group's photo.
    SetIcon {
        chat: ChatRef,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        group_version: u64,
    },
    /// Fetch one attachment's bytes to a local file.
    ///
    /// Named by the message guid and the attachment's position within it,
    /// rather than by handing the attachment object back.
    ///
    /// The object cannot make the round trip: its MMCS locator and key are
    /// `Vec<u8>` fields serialized as plist `Data`, and JSON renders those as
    /// a sequence of numbers, which `bin_deserialize` rejects — "invalid
    /// type: sequence, expected a byte array". So the helper keeps the
    /// attachments of recent messages instead, and the caller refers to them.
    DownloadAttachment {
        guid: String,
        /// Index among that message's attachments, in the order they appear
        /// in its parts — the same order the Python side enumerates them.
        index: usize,
        /// Absolute path to write. The caller owns naming and cleanup.
        dest: String,
    },
    /// Add or remove a tapback. `emoji` selects an arbitrary-emoji tapback
    /// (iOS 18+); otherwise `kind` picks one of the six classic ones.
    React {
        chat: ChatRef,
        target_guid: String,
        /// Text of the target message. iMessage carries it in the tapback so
        /// recipients can render 'Loved "…"' without a lookup.
        #[serde(default)]
        target_text: String,
        #[serde(default)]
        target_part: Option<u64>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        emoji: Option<String>,
        #[serde(default = "yes")]
        enable: bool,
    },
    Edit {
        chat: ChatRef,
        target_guid: String,
        #[serde(default)]
        part: u64,
        text: String,
        /// Formatting ranges over `text`, in UTF-16 code units, as on `send`.
        #[serde(default)]
        format: Vec<compose::FormatRange>,
    },
    Unsend {
        chat: ChatRef,
        target_guid: String,
        #[serde(default)]
        part: u64,
    },
    Typing {
        chat: ChatRef,
        typing: bool,
    },
    /// Mark the thread read on every other device on the account.
    MarkRead {
        chat: ChatRef,
        /// Guid of the newest message being marked read. Without it the
        /// receipt carries a random id, which the sender's phone cannot match
        /// to any message, so it never shows "Read".
        #[serde(default)]
        guid: Option<String>,
    },
    /// Send a read receipt for one message to its sender (and sync it to
    /// our other devices). Same wire message as `mark_read` with `guid`.
    ReadReceipt {
        chat: ChatRef,
        guid: String,
    },
    Rename {
        chat: ChatRef,
        name: String,
    },
    /// Send a native iOS 26 poll. See `poll.rs`.
    SendPoll {
        chat: ChatRef,
        #[serde(default)]
        question: String,
        options: Vec<String>,
    },
    /// Set our whole selection on a poll; an empty list withdraws.
    PollVote {
        chat: ChatRef,
        poll_guid: String,
        option_ids: Vec<String>,
        /// The poll's balloon session id. Optional when the poll was sent or
        /// received since this helper started.
        #[serde(default)]
        session: Option<String>,
    },
    /// Add an option to an existing poll, as an iPhone does.
    PollAddOption {
        chat: ChatRef,
        poll_guid: String,
        text: String,
        #[serde(default)]
        session: Option<String>,
    },
    /// Send a URL as a rich link card; see `link.rs`.
    SendLink {
        chat: ChatRef,
        url: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        site_name: Option<String>,
        /// Local JPEG or PNG for the card image.
        #[serde(default)]
        image_path: Option<String>,
        /// Fetch the page's OpenGraph tags to fill a missing title or image.
        #[serde(default = "yes")]
        fetch: bool,
    },
    /// Write the preview image of a received link card to a file.
    DownloadLinkImage { guid: String, dest: String },
    /// Shared Focus state of the given handles (all sharers if empty).
    FocusStatus {
        #[serde(default)]
        handles: Vec<String>,
    },
    /// Set (`path`) or clear (no `path`) a chat background.
    SetBackground {
        chat: ChatRef,
        /// A ready transcript-background payload (zip).
        #[serde(default)]
        path: Option<String>,
        /// Any JPEG, PNG or HEIC; built into a photo background.
        #[serde(default)]
        image_path: Option<String>,
        #[serde(default)]
        gradient: Option<GradientSpec>,
        /// A captured dynamic preset: aurora_2, clouds_2, ocean_2.
        #[serde(default)]
        dynamic: Option<String>,
        #[serde(default)]
        version: Option<u64>,
    },
    /// Fetch a received background's payload (and its preview image).
    DownloadBackground { guid: String, dest: String },
    /// Replaces the participant list wholesale, which is how iMessage models
    /// both adding and removing.
    SetParticipants {
        chat: ChatRef,
        participants: Vec<String>,
        #[serde(default)]
        group_version: u64,
    },
}

/// `set_background` gradient: two colors, `"#RRGGBB"` or `[r,g,b(,a)]` in 0..1.
#[derive(Deserialize)]
struct GradientSpec {
    colors: Vec<Json>,
}

/// A local file to send. `mime` and `uti` default from the extension.
#[derive(Deserialize)]
struct AttachmentSpec {
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    uti: Option<String>,
}

fn yes() -> bool {
    true
}

/// A single unformatted text part.
///
/// `MessageParts::from_raw` does exactly this but is private, and the parts
/// vector is how iMessage represents a message body: one entry per run of
/// text, attachment or mention. An edit replaces one part, which is why edits
/// need this rather than a bare string.
fn plain_text(text: &str) -> MessageParts {
    MessageParts(vec![IndexedMessagePart {
        part: MessagePart::Text(text.to_string(), Default::default()),
        idx: None,
        ext: None,
    }])
}

#[derive(Serialize)]
struct Reply {
    id: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    data: Json,
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

/// Recent messages' attachments, oldest evicted first.
///
/// Sized for "long enough that the daemon's download, which starts the moment
/// the message is published, still finds it" — not for history. A miss is
/// reported rather than papered over, because the alternative is a download
/// that silently produces nothing.
#[derive(Default)]
struct AttachmentCache {
    by_guid: HashMap<String, Vec<rustpush::Attachment>>,
    order: std::collections::VecDeque<String>,
}

impl AttachmentCache {
    const LIMIT: usize = 256;

    fn insert(&mut self, guid: String, atts: Vec<rustpush::Attachment>) {
        if self.by_guid.contains_key(&guid) {
            return;
        }
        while self.order.len() >= Self::LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.by_guid.remove(&old);
            }
        }
        self.order.push_back(guid.clone());
        self.by_guid.insert(guid, atts);
    }

    fn get(&self, guid: &str) -> Option<&Vec<rustpush::Attachment>> {
        self.by_guid.get(guid)
    }
}

/// The attachments of a message, in the order they appear among its parts —
/// which is the order the Python side enumerates them, and therefore what the
/// `index` on `DownloadAttachment` refers to.
fn attachments_of(inst: &MessageInst) -> Vec<rustpush::Attachment> {
    match &inst.message {
        Message::Message(normal) => normal
            .parts
            .0
            .iter()
            .filter_map(|p| match &p.part {
                rustpush::MessagePart::Attachment(a) => Some(a.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

struct Session {
    client: IMClient,
    /// Kept alongside the client because attachment downloads need the APNs
    /// connection directly — `Attachment::get_attachment` talks to MMCS
    /// rather than going through `IMClient`.
    conn: rustpush::APSConnection,
    /// Attachments of recently published messages, by guid.
    ///
    /// Exists because the attachment object cannot survive a JSON round trip
    /// (see `DownloadAttachment`), so the daemon refers to one by position
    /// instead of holding it. Bounded: the helper is long-lived and every
    /// photo that arrives would otherwise be retained for good.
    attachments: Mutex<AttachmentCache>,
    handles: Vec<String>,
    /// Broadcast so every connected client sees every event; the daemon and a
    /// debugging `socat` can watch at the same time.
    events: broadcast::Sender<String>,
    /// Sends mutate shared identity state, so they go one at a time.
    send_lock: Mutex<()>,
    warnings: Vec<String>,
    /// Optional override for the sending identity; see `my_handle`.
    send_as: Option<String>,
    /// Our shared Name and Photo; see `profile.rs`. Behind a lock because
    /// `set_profile` replaces it while the helper runs.
    profile: Mutex<Option<profile::Profile>>,
    /// Apple ID sign-in state for `set_profile`, including a pending 2FA.
    profile_auth: Mutex<setprofile::Auth>,
    state_dir: PathBuf,
    os_config: Arc<dyn rustpush::OSConfig>,
    share_mode: profile::ShareMode,
    /// Acknowledge incoming messages; off with `--no-delivery-receipts`.
    delivery_receipts: bool,
    /// Display name recorded for a profile adopted from our own device,
    /// whose update carries no name. `--profile-name`, default "Group G".
    profile_name: String,
    /// Conversations that already got the profile, for `--share-profile first`.
    profile_sent: Mutex<profile::Sent>,
    /// StatusKit receiver, with `--focus` only; see `focus.rs`.
    focus: Option<Arc<focus::Focus>>,
    /// Why Focus is off, for `status` and `focus_status` errors.
    focus_note: String,
}

impl Session {
    /// The handle we send as.
    ///
    /// Prefers the phone number over any email address. This is not cosmetic:
    /// the recipient's phone threads incoming messages by the address they
    /// came from, so sending as an email starts a *second* conversation
    /// alongside the one their iPhone already has with your number — the same
    /// person appearing twice on their device.
    ///
    /// Registration lists emails first, so taking `handles.first()` picked an
    /// email and did exactly that.
    ///
    /// `--send-as` overrides this when the registration has several numbers,
    /// or when sending from an email really is what's wanted.
    fn my_handle(&self) -> Result<&str> {
        if let Some(preferred) = self.send_as.as_deref() {
            if let Some(found) = self.handles.iter().find(|h| h.as_str() == preferred) {
                return Ok(found.as_str());
            }
            log::warn!(
                "--send-as {preferred} is not a registered handle; \
                 falling back to the default"
            );
        }
        self.handles
            .iter()
            .find(|h| h.starts_with("tel:"))
            .or_else(|| self.handles.first())
            .map(|s| s.as_str())
            .ok_or_else(|| anyhow!("no registered handles — registration has expired"))
    }

    async fn dispatch(&self, id: Option<String>, cmd: Command) -> Result<Json> {
        match cmd {
            Command::Status => {
                // Snapshot first: holding the lock inside `json!` while
                // locking it again would deadlock.
                let prof = self.profile.lock().await.as_ref().map(|p| {
                    (p.display_name.clone(), p.message.cloud_kit_record_key.clone(), p.source.clone())
                });
                Ok(json!({
                "handles": self.handles,
                // Which identity messages actually go out as. Surfaced
                // because sending from the wrong one silently splits the
                // conversation on the recipient's phone, and there is no
                // other way to notice from this side.
                "send_as": self.my_handle().unwrap_or("<none>"),
                "warnings": self.warnings,
                "note": SINGLETON_NOTE,
                "profile": {
                    "found": prof.as_ref().is_some(),
                    "display_name": prof.as_ref().and_then(|p| p.0.clone()),
                    "record_key": prof.as_ref().map(|p| p.1.clone()),
                    "source": prof.as_ref().and_then(|p| p.2.clone()),
                    "share": self.share_mode.as_str(),
                },
                "focus": { "enabled": self.focus.is_some(), "note": self.focus_note },
            }))
            }
            Command::Handles => Ok(json!({ "handles": self.handles })),

            Command::Send {
                chat,
                text,
                subject,
                reply_guid,
                reply_part,
                effect,
                format,
                mentions,
                attachments,
                as_audio_message,
            } => {
                if text.is_empty() && attachments.is_empty() {
                    bail!("send needs `text`, `attachments`, or both");
                }
                if text.is_empty() && !(format.is_empty() && mentions.is_empty()) {
                    bail!("`format` and `mentions` need `text`");
                }
                if as_audio_message && (attachments.len() != 1 || !text.is_empty()) {
                    bail!("`as_audio_message` needs exactly one attachment and no text");
                }
                // Validate every file before uploading any, so a bad path
                // late in the list doesn't cost the uploads ahead of it.
                for (i, a) in attachments.iter().enumerate() {
                    let meta = std::fs::metadata(&a.path)
                        .with_context(|| format!("attachment {i}: {}", a.path))?;
                    if !meta.is_file() {
                        bail!("attachment {i}: {} is not a file", a.path);
                    }
                }
                // Text first, then attachments, which is the order iOS lays
                // a mixed message out in.
                let mut parts = if text.is_empty() {
                    Vec::new()
                } else {
                    compose::text_parts(&text, &format, &mentions)?
                };
                let count = attachments.len();
                for (i, a) in attachments.iter().enumerate() {
                    let att = self
                        .upload_attachment(id.as_deref(), i, count, a)
                        .await
                        .with_context(|| format!("uploading attachment {i} ({})", a.path))?;
                    parts.push(IndexedMessagePart {
                        part: MessagePart::Attachment(att),
                        idx: None,
                        ext: None,
                    });
                }
                let mut normal = NormalMessage::new(String::new(), MessageType::IMessage);
                normal.parts = MessageParts(parts);
                normal.voice = as_audio_message;
                normal.subject = subject;
                normal.embedded_profile = self.profile_for(&chat).await;
                // rustpush builds the reply field as `r:<part>:<guid>` and
                // unwraps `reply_part` unconditionally whenever `reply_guid`
                // is set (messages.rs:2261). Passing a guid without a part
                // therefore panics the worker task, and because the panic
                // kills the task before it can answer, the caller sees a
                // D-Bus timeout rather than an error. Default the part so
                // that combination can't be constructed here.
                if reply_guid.is_some() {
                    normal.reply_part = Some(reply_part.unwrap_or_else(|| "0".to_string()));
                } else {
                    normal.reply_part = None;
                }
                normal.reply_guid = reply_guid;
                normal.effect = effect;
                self.send(chat, Message::Message(normal)).await
            }

            Command::React {
                chat,
                target_guid,
                target_text,
                target_part,
                kind,
                emoji,
                enable,
            } => {
                // Case-folded: the verb names are also what iMessage shows in
                // its menu ("Heart", "Like"), so callers naturally send them
                // capitalised. Matching only lowercase rejected every tapback
                // the UI sent, with the error arriving too late to be visible.
                let kind = kind.map(|k| k.to_ascii_lowercase());
                let reaction = match (emoji, kind.as_deref()) {
                    (Some(e), _) => Reaction::Emoji(e),
                    (None, Some("heart") | Some("love") | Some("loved")) => Reaction::Heart,
                    (None, Some("like") | Some("liked")) => Reaction::Like,
                    (None, Some("dislike") | Some("disliked")) => Reaction::Dislike,
                    (None, Some("laugh") | Some("laughed")) => Reaction::Laugh,
                    (None, Some("emphasize") | Some("emphasized")) => Reaction::Emphasize,
                    (None, Some("question") | Some("questioned")) => Reaction::Question,
                    (None, other) => bail!(
                        "unknown tapback {:?}; expected one of heart/like/dislike/\
                         laugh/emphasize/question, or an `emoji`",
                        other.unwrap_or("<none>")
                    ),
                };
                // Classic tapbacks (and emoji ones) address a *part* of the
                // target message. rustpush only writes the wire form
                // `p:{part}/{uuid}` when `to_part` is set; with None it sends
                // a bare uuid, which Apple accepts but no client attaches as a
                // tapback — so reactions appeared to send and then vanished
                // on every device, including the phone. Part 0 is the text
                // body of an ordinary message; callers that know a richer
                // multi-part layout can still pass an explicit index.
                let react = ReactMessage {
                    to_uuid: target_guid,
                    to_part: Some(target_part.unwrap_or(0)),
                    reaction: ReactMessageType::React { reaction, enable },
                    to_text: target_text,
                    embedded_profile: None,
                };
                self.send(chat, Message::React(react)).await
            }

            Command::SendPoll { chat, question, options } => {
                let built = poll::build_poll(self.my_handle()?, &question, &options)?;
                let mut reply = self.send(chat, Message::Message(built.message)).await?;
                let guid = reply["guid"].as_str().unwrap_or_default().to_string();
                let ids: Vec<String> = built.options.iter().map(|(i, _)| i.clone()).collect();
                poll::register_sent(&guid, &built.session, built.options, built.payload);
                reply["option_ids"] = json!(ids);
                reply["session"] = json!(built.session);
                Ok(reply)
            }

            Command::PollVote { chat, poll_guid, option_ids, session } => {
                // An update guid names the same poll.
                let poll_guid = poll::canonical(&poll_guid);
                let session = session
                    .or_else(|| poll::session_of(&poll_guid))
                    .ok_or_else(|| anyhow!(
                        "poll {poll_guid} has not been seen since the helper started; pass `session`"
                    ))?;
                if let Some(known) = poll::option_ids_of(&poll_guid) {
                    if let Some(bad) = option_ids.iter().find(|i| !known.contains(i)) {
                        bail!("option {bad} is not on poll {poll_guid}");
                    }
                }
                // iOS votes on the poll's newest message (an update, once an
                // option has been added); see poll::latest_of.
                let target = poll::latest_of(&poll_guid);
                let vote = poll::build_vote(&target, &session, self.my_handle()?, &option_ids)?;
                let mut reply = self.send(chat, Message::React(vote)).await?;
                reply["poll_guid"] = json!(poll_guid);
                reply["target_guid"] = json!(target);
                Ok(reply)
            }

            Command::PollAddOption { chat, poll_guid, text, session } => {
                let poll_guid = poll::canonical(&poll_guid);
                let session = session
                    .or_else(|| poll::session_of(&poll_guid))
                    .ok_or_else(|| anyhow!(
                        "poll {poll_guid} has not been seen since the helper started; pass `session`"
                    ))?;
                let base = poll::payload_of(&poll_guid).ok_or_else(|| anyhow!(
                    "the options of poll {poll_guid} are not known (not seen since the helper started)"
                ))?;
                let built = poll::build_add_option(&poll_guid, &session, self.my_handle()?, &base, &text)?;
                let mut reply = self.send(chat, Message::React(built.react)).await?;
                let update_guid = reply["guid"].as_str().unwrap_or_default().to_string();
                poll::register_update(&poll_guid, &update_guid, &built.payload);
                reply["poll_guid"] = json!(poll_guid);
                reply["option_id"] = json!(built.option_id);
                Ok(reply)
            }

            Command::Edit {
                chat,
                target_guid,
                part,
                text,
                format,
            } => {
                // Formatted runs, as a send builds them; plain text otherwise.
                let new_parts = if format.is_empty() {
                    plain_text(&text)
                } else {
                    MessageParts(compose::text_parts(&text, &format, &[])?)
                };
                self.send(
                    chat,
                    Message::Edit(EditMessage {
                        tuuid: target_guid,
                        edit_part: part,
                        new_parts,
                    }),
                )
                .await
            }

            Command::Unsend {
                chat,
                target_guid,
                part,
            } => {
                self.send(
                    chat,
                    Message::Unsend(UnsendMessage {
                        tuuid: target_guid,
                        edit_part: part,
                    }),
                )
                .await
            }

            Command::Typing { chat, typing } => {
                // The `Option<TypingApp>` names a third-party iMessage app
                // that's driving the indicator. None means Messages itself,
                // which is what we want to look like.
                self.send(chat, Message::Typing(typing, None)).await
            }

            Command::MarkRead { chat, guid } => self.send_read(chat, guid).await,
            Command::ReadReceipt { chat, guid } => self.send_read(chat, Some(guid)).await,

            Command::SetProfile {
                name,
                first,
                last,
                photo,
                tfa_code,
            } => {
                if name.trim().is_empty() {
                    bail!("set_profile needs a non-empty `name`");
                }
                // Prepared before signing in, so a bad photo costs nothing.
                let photo = photo.as_deref().map(setprofile::prepare_photo).transpose()?;
                let current = self.profile.lock().await.as_ref().map(|p| p.message.clone());
                let mut auth = self.profile_auth.lock().await;
                let outcome = setprofile::set_profile(
                    &self.os_config,
                    &self.conn,
                    &self.state_dir,
                    &mut auth,
                    current,
                    rustpush::name_photo_sharing::IMessageNameRecord { name, first, last },
                    photo,
                    tfa_code.as_deref(),
                )
                .await?;
                match outcome {
                    setprofile::Outcome::Needs2fa(why) => bail!(
                        "2fa_required: {why}; call set_profile again with the same \
                         fields plus \"tfa_code\""
                    ),
                    setprofile::Outcome::Done(p) => {
                        let reply = json!({
                            "found": true,
                            "display_name": p.display_name,
                            "record_key": p.message.cloud_kit_record_key,
                            "source": p.source,
                            "share": self.share_mode.as_str(),
                        });
                        // Live: the next send carries the new record.
                        *self.profile.lock().await = Some(p);
                        Ok(reply)
                    }
                }
            }

            Command::SetIcon {
                chat,
                path,
                group_version,
            } => {
                // No path means "remove the photo": rustpush sends the
                // IconChange with no file and `meta: "ngp"`.
                let file = match path {
                    None => None,
                    Some(p) => Some(
                        self.upload_mmcs(id.as_deref(), 0, 1, &p)
                            .await
                            .with_context(|| format!("uploading group photo {p}"))?,
                    ),
                };
                self.send(
                    chat,
                    Message::IconChange(rustpush::IconChangeMessage { file, group_version }),
                )
                .await
            }

            Command::Rename { chat, name } => {
                self.send(chat, Message::RenameMessage(RenameMessage { new_name: name }))
                    .await
            }

            Command::DownloadAttachment { guid, index, dest } => {
                let attachment = {
                    let cache = self.attachments.lock().await;
                    cache
                        .get(&guid)
                        .and_then(|v| v.get(index))
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!(
                                "no attachment {index} cached for {guid}; the \
                                 message is older than the helper or has been \
                                 evicted"
                            )
                        })?
                };
                let path = PathBuf::from(&dest);
                // Written through a temp file in the same directory and then
                // renamed: the daemon treats "the file exists" as "the image
                // is ready", so a half-written file would be handed to the UI
                // as a truncated, undecodable image.
                let tmp = path.with_extension("part");
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let size = {
                    let mut file = std::fs::File::create(&tmp)
                        .with_context(|| format!("creating {}", tmp.display()))?;
                    attachment
                        .get_attachment(&self.conn, &mut file, |_, _| {})
                        .await
                        .context("downloading attachment from MMCS")?;
                    file.metadata().map(|m| m.len()).unwrap_or(0)
                };
                std::fs::rename(&tmp, &path)
                    .with_context(|| format!("renaming into {}", path.display()))?;
                Ok(json!({ "path": dest, "bytes": size }))
            }

            Command::SendLink { chat, url, title, summary, site_name, image_path, fetch } => {
                link::check_url(&url)?;
                let (card, card_err) = self
                    .build_link(&url, title, summary, site_name, image_path, fetch)
                    .await;
                let profile = self.profile_for(&chat).await;
                if let Some(mut m) = card {
                    m.embedded_profile = profile.clone();
                    match self.send(chat.clone(), Message::Message(m)).await {
                        Ok(mut r) => {
                            r["card"] = json!(true);
                            r["card_note"] = json!(card_err);
                            return Ok(r);
                        }
                        Err(e) => {
                            // A failed card send is retried as plain text. If the
                            // card had reached some devices, they see the URL twice.
                            log::warn!("link card send failed, sending plain URL: {e:#}");
                            let mut plain = link::plain(&url);
                            plain.embedded_profile = profile;
                            let mut r = self.send(chat, Message::Message(plain)).await?;
                            r["card"] = json!(false);
                            r["card_error"] = json!(format!("{e:#}"));
                            return Ok(r);
                        }
                    }
                }
                let mut plain = link::plain(&url);
                plain.embedded_profile = profile;
                let mut r = self.send(chat, Message::Message(plain)).await?;
                r["card"] = json!(false);
                r["card_error"] = json!(card_err);
                Ok(r)
            }

            Command::DownloadLinkImage { guid, dest } => {
                let (mime, bytes) = link::image_of(&guid).ok_or_else(|| {
                    anyhow!("no link preview image cached for {guid}; it is older than the helper, evicted, or had none")
                })?;
                write_atomic(&PathBuf::from(&dest), &bytes)?;
                Ok(json!({ "path": dest, "bytes": bytes.len(), "mime": mime }))
            }

            Command::FocusStatus { handles } => {
                let Some(f) = &self.focus else {
                    bail!("Focus status is off: {}", self.focus_note);
                };
                let handles = if handles.is_empty() { f.sharers().await } else { handles };
                Ok(f.query(&handles).await)
            }

            Command::SetBackground { chat, path, image_path, gradient, dynamic, version } => {
                // `cid` = the chat guid. [VERIFY that iOS keys it the same way]
                let chat_id = chat.guid.clone();
                let given = [path.is_some(), image_path.is_some(), gradient.is_some(), dynamic.is_some()]
                    .iter()
                    .filter(|b| **b)
                    .count();
                if given > 1 {
                    bail!("set_background takes one of `path`, `image_path`, `gradient`, `dynamic` (none clears)");
                }
                // Build first, so a bad image costs no upload.
                let built: Option<(Vec<u8>, &str)> = if let Some(img) = &image_path {
                    let img = img.clone();
                    let (heic, png, d) = tokio::task::spawn_blocking(move || bgbuild::prep_photo(&img))
                        .await
                        .map_err(|e| anyhow!("photo prep task: {e}"))??;
                    Some((bgbuild::build_photo(heic, png, d)?, "photo"))
                } else if let Some(g) = &gradient {
                    let colors = g.colors.iter().map(bgbuild::parse_color).collect::<Result<Vec<_>>>()?;
                    Some((bgbuild::build_gradient(&colors)?, "gradient"))
                } else if let Some(p) = &dynamic {
                    Some((bgbuild::build_dynamic(p)?, "dynamic"))
                } else {
                    None
                };
                let tmp = std::env::temp_dir().join(format!("ib-bg-{}.zip", uuid::Uuid::new_v4()));
                let upload_path = match (&built, &path) {
                    (Some((bytes, _)), _) => {
                        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
                        Some(tmp.display().to_string())
                    }
                    (None, Some(p)) => Some(p.clone()),
                    (None, None) => None,
                };
                let kind = match (&built, &path) {
                    (Some((_, k)), _) => Some(k.to_string()),
                    (None, Some(p)) => Some(background::validate_payload(p)?),
                    _ => None,
                };
                let version = background::next_version(chat_id.as_deref(), version);
                let file = match &upload_path {
                    None => None,
                    Some(p) => {
                        let r = self.upload_mmcs(id.as_deref(), 0, 1, p).await;
                        let _ = std::fs::remove_file(&tmp);
                        Some(r.with_context(|| format!("uploading background {p}"))?)
                    }
                };
                let cleared = file.is_none();
                let msg = background::build(file, version, chat_id.clone());
                let mut r = self.send(chat, Message::SetTranscriptBackground(msg)).await?;
                background::note_version(chat_id.as_deref(), version);
                r["version"] = json!(version);
                r["cleared"] = json!(cleared);
                r["poster"] = json!(kind);
                Ok(r)
            }

            Command::DownloadBackground { guid, dest } => {
                let msg = background::recent(&guid).ok_or_else(|| {
                    anyhow!("no background set cached for {guid}; it is older than the helper or was a clear")
                })?;
                let mmcs = msg.to_mmcs().ok_or_else(|| anyhow!("{guid} cleared the background; nothing to download"))?;
                let mut buf = std::io::Cursor::new(Vec::new());
                mmcs.get_attachment(&self.conn, &mut buf, |_, _| {})
                    .await
                    .map_err(|e| anyhow!("downloading background from MMCS: {e}"))?;
                let bytes = buf.into_inner();
                let path = PathBuf::from(&dest);
                write_atomic(&path, &bytes)?;
                let (preview, kind) = background::extract_preview(&bytes, &path);
                Ok(json!({ "path": dest, "bytes": bytes.len(), "preview": preview, "poster": kind }))
            }

            Command::SetParticipants {
                chat,
                participants,
                group_version,
            } => {
                self.send(
                    chat,
                    Message::ChangeParticipants(rustpush::ChangeParticipantMessage {
                        new_participants: participants,
                        group_version,
                    }),
                )
                .await
            }
        }
    }

    async fn send(&self, chat: ChatRef, message: Message) -> Result<Json> {
        let _guard = self.send_lock.lock().await;
        let conversation: ConversationData = chat.into();
        // `IMClient::to_message` doesn't exist for outbound traffic — that
        // helper hangs off `IDSRecvMessage`, for building a reply in the
        // context of something received. Composing fresh means constructing
        // the instance ourselves and naming the handle we're sending as.
        let mut inst = MessageInst::new(conversation, self.my_handle()?, message);
        // Report the guid before waiting on delivery: the UI wants to show the
        // bubble immediately, and per-recipient delivery is asynchronous.
        let guid = inst.id.clone();
        self.client
            .send(&mut inst)
            .await
            .context("sending to Apple")?;
        // rustpush mints the conversation guid in `prepare_send` when the
        // caller gave none (which is how a new group comes into being), so
        // it is read back here for the caller to keep.
        let chat_guid = inst
            .conversation
            .as_ref()
            .and_then(|c| c.sender_guid.clone());
        Ok(json!({ "guid": guid, "chat_guid": chat_guid }))
    }

    /// Adopt the Share Name and Photo profile our own other device (the
    /// account's Mac or iPhone) announces.
    ///
    /// When Name & Photo Sharing is set up on an Apple device, it sends
    /// `UpdateProfile` (rustpush logs it as "Updated their profile") to the
    /// account's other devices, carrying the CloudKit record id and the
    /// decryption key — exactly what outgoing messages embed. `ShareProfile`
    /// carries the same pair. Only messages whose sender is one of OUR
    /// registered handles are adopted: anyone else's is that contact's
    /// profile and is only published as an event.
    ///
    /// The update has no display name. Reading it from the record needs
    /// `ProfilesClient::get_record`, which needs a signed-in CloudKit client,
    /// and sign-in is what Apple refused (-80045). So the name recorded is
    /// `--profile-name`. It only labels `status`; recipients see the name in
    /// the record.
    async fn maybe_adopt_profile(&self, inst: &MessageInst) {
        let from_me = inst.sender.as_ref().is_some_and(|s| self.handles.contains(s));
        if !from_me {
            return;
        }
        let announced = match &inst.message {
            Message::UpdateProfile(u) => u.profile.clone(),
            Message::ShareProfile(m) => Some(m.clone()),
            _ => return,
        };
        let Some(message) = announced else {
            // Our own device deleted the profile: stop sharing it.
            if self.profile.lock().await.take().is_some() {
                match profile::remove(&self.state_dir) {
                    Ok(()) => log::info!("own device deleted the shared profile; dropped it"),
                    Err(e) => log::warn!("could not remove {}: {e}", profile::FILE),
                }
            }
            return;
        };
        let mut current = self.profile.lock().await;
        let same = current.as_ref().is_some_and(|p| {
            p.message.cloud_kit_record_key == message.cloud_kit_record_key
                && p.message.cloud_kit_decryption_record_key == message.cloud_kit_decryption_record_key
        });
        if same {
            // Apple devices send the update several times; once is enough.
            return;
        }
        let source = "own device";
        if let Err(e) = profile::save(&self.state_dir, &message, Some(&self.profile_name), source) {
            log::error!("adopting shared profile from own device: {e:#}");
            return;
        }
        log::info!(
            "adopted shared profile from own device ({}, record {})",
            inst.sender.as_deref().unwrap_or("?"),
            message.cloud_kit_record_key
        );
        *current = Some(profile::Profile {
            message,
            display_name: Some(self.profile_name.clone()),
            source: Some(source.to_string()),
        });
    }

    /// The link card for `send_link`, or None plus the reason it could not
    /// be built. Given fields win; OpenGraph fills only what is missing.
    async fn build_link(
        &self,
        url: &str,
        title: Option<String>,
        summary: Option<String>,
        site_name: Option<String>,
        image_path: Option<String>,
        fetch: bool,
    ) -> (Option<NormalMessage>, Option<String>) {
        let mut notes = Vec::new();
        let mut card = link::Card { title, summary, site_name, image_url: None };
        let mut image = None;
        if let Some(p) = &image_path {
            match std::fs::read(p).map_err(anyhow::Error::from).and_then(|b| link::CardImage::from_bytes(b, None)) {
                Ok(i) => image = Some(i),
                Err(e) => notes.push(format!("image {p}: {e:#}")),
            }
        }
        if fetch && (card.title.is_none() || image.is_none()) {
            match link::fetch(url).await {
                Ok((og, og_image)) => {
                    card.title = card.title.or(og.title);
                    card.summary = card.summary.or(og.summary);
                    card.site_name = card.site_name.or(og.site_name);
                    card.image_url = og.image_url.clone();
                    if image.is_none() {
                        if let Some(b) = og_image {
                            match link::CardImage::from_bytes(b, og.image_url) {
                                Ok(i) => image = Some(i),
                                Err(e) => notes.push(format!("og:image: {e:#}")),
                            }
                        }
                    }
                }
                Err(e) => notes.push(format!("fetch: {e:#}")),
            }
        }
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        if card.title.is_none() && image.is_none() {
            return (None, Some(note.unwrap_or_else(|| "no title or image to show".into())));
        }
        (Some(link::build(url, &card, image)), note)
    }

    /// A `Read` receipt. iMessage matches a read receipt to a message by the
    /// receipt's own id, so it must be the guid of the message read.
    async fn send_read(&self, chat: ChatRef, guid: Option<String>) -> Result<Json> {
        let _guard = self.send_lock.lock().await;
        let conversation: ConversationData = chat.into();
        let mut inst = MessageInst::new(conversation, self.my_handle()?, Message::Read);
        if let Some(g) = guid {
            inst.id = g.to_uppercase();
        }
        let id = inst.id.clone();
        self.client.send(&mut inst).await.context("sending read receipt")?;
        let chat_guid = inst.conversation.as_ref().and_then(|c| c.sender_guid.clone());
        Ok(json!({ "guid": id, "chat_guid": chat_guid }))
    }

    /// Acknowledge an incoming message the way a device does, so the sender
    /// sees "Delivered".
    ///
    /// rustpush only certifies delivery for messages it could not parse
    /// (aps_client.rs `process_msg`); a parsed one is left unacknowledged,
    /// which is why nothing showed "Delivered". Two mechanisms, as the
    /// sender's payload dictates:
    /// - Certified delivery (current iOS): the payload carries a
    ///   `certified_context`. Answer with `certify_delivery`; `notify` asks
    ///   Apple to generate the "Delivered" receipt (`gd`) and is set only
    ///   when the sender requested one. Every certified message is acked.
    /// - Legacy: the payload sets `send_delivered` with no certified context.
    ///   Answer with a `Message::Delivered` carrying the message's guid,
    ///   addressed to the sender's device token only.
    ///
    /// Senders request delivered receipts for 1:1 messages and for tapbacks
    /// in groups (rustpush `should_send_delivered`, mirroring iOS), so group
    /// messages are acked but iOS shows no "Delivered" for them, as on a
    /// real phone.
    async fn acknowledge(&self, inst: MessageInst) {
        let from_me = inst.sender.as_ref().map_or(true, |s| self.handles.contains(s));
        if from_me {
            return;
        }
        let is_user_message = matches!(
            inst.message,
            Message::Message(_) | Message::React(_) | Message::Edit(_) | Message::Unsend(_)
                | Message::RenameMessage(_) | Message::ChangeParticipants(_)
                | Message::IconChange(_)
        );
        if !is_user_message {
            return;
        }
        if let Some(ctx) = &inst.certified_context {
            if let Err(e) = self
                .client
                .identity
                .certify_delivery("com.apple.madrid", ctx, inst.send_delivered)
                .await
            {
                log::warn!("certifying delivery of {}: {e}", inst.id);
            } else {
                log::debug!("certified delivery of {} (notify {})", inst.id, inst.send_delivered);
            }
            return;
        }
        if !inst.send_delivered {
            return;
        }
        let Some(sender) = inst.sender.clone() else { return };
        // Send as whichever of our handles the message was addressed to.
        let me = inst
            .conversation
            .as_ref()
            .and_then(|c| c.participants.iter().find(|p| self.handles.contains(*p)).cloned())
            .or_else(|| self.my_handle().ok().map(str::to_string));
        let Some(me) = me else { return };
        let mut receipt = MessageInst {
            id: inst.id.clone(),
            sender: Some(me),
            conversation: Some(ConversationData {
                participants: vec![sender],
                cv_name: None,
                sender_guid: inst.conversation.as_ref().and_then(|c| c.sender_guid.clone()),
                after_guid: None,
            }),
            message: Message::Delivered,
            sent_timestamp: 0,
            // The sender's own push token, so only the sending device gets it.
            target: inst.target.clone(),
            send_delivered: false,
            verification_failed: false,
            certified_context: None,
        };
        let _guard = self.send_lock.lock().await;
        if let Err(e) = self.client.send(&mut receipt).await {
            log::warn!("sending delivered receipt for {}: {e}", inst.id);
        }
    }

    /// The `embedded_profile` for a message to `chat` under `--share-profile`.
    async fn profile_for(&self, chat: &ChatRef) -> Option<rustpush::ShareProfileMessage> {
        let guard = self.profile.lock().await;
        let p = guard.as_ref()?;
        match self.share_mode {
            profile::ShareMode::Never => None,
            profile::ShareMode::Always => Some(p.message.clone()),
            profile::ShareMode::First => self
                .profile_sent
                .lock()
                .await
                .first_time(chat.guid.as_deref(), &chat.participants)
                .then(|| p.message.clone()),
        }
    }

    /// Encrypt and upload one file to MMCS, emitting `upload_progress` events
    /// tagged with the request id.
    async fn upload_mmcs(
        &self,
        id: Option<&str>,
        index: usize,
        count: usize,
        path: &str,
    ) -> Result<rustpush::MMCSFile> {
        // Two passes over the file: `prepare_put` hashes and chunks it, then
        // the upload reads it again. Re-opened rather than seeked so neither
        // pass depends on the other's read position.
        let first = std::fs::File::open(path).with_context(|| format!("opening {path}"))?;
        let prepared = rustpush::MMCSFile::prepare_put(first)
            .await
            .map_err(|e| anyhow!("preparing upload: {e}"))?;
        let reader = std::fs::File::open(path).with_context(|| format!("opening {path}"))?;
        let events = self.events.clone();
        let id = id.map(str::to_string);
        let mut last_decile: i64 = -1;
        rustpush::MMCSFile::new(&self.conn, &prepared, reader, move |done, total| {
            let pct = if total == 0 { 100 } else { (done * 100 / total) as i64 };
            // Every 10%, not every chunk: a large video would otherwise flood
            // every connected client.
            if pct / 10 != last_decile {
                last_decile = pct / 10;
                let _ = events.send(
                    json!({
                        "event": "upload_progress",
                        "id": id,
                        "index": index,
                        "count": count,
                        "bytes": done,
                        "total": total,
                    })
                    .to_string(),
                );
            }
        })
        .await
        .map_err(|e| anyhow!("MMCS upload: {e}"))
    }

    async fn upload_attachment(
        &self,
        id: Option<&str>,
        index: usize,
        count: usize,
        spec: &AttachmentSpec,
    ) -> Result<rustpush::Attachment> {
        let path = std::path::Path::new(&spec.path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let (mime, uti) = compose::mime_uti(ext);
        let name = spec
            .name
            .clone()
            .or_else(|| path.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .unwrap_or_else(|| "file".to_string());
        let mmcs = self.upload_mmcs(id, index, count, &spec.path).await?;
        // The same record `Attachment::new_mmcs` builds after its upload;
        // built here so the upload can share `upload_mmcs`'s progress events.
        Ok(rustpush::Attachment {
            a_type: rustpush::AttachmentType::MMCS(vec![mmcs]),
            part: index as u64,
            uti_type: spec.uti.clone().unwrap_or_else(|| uti.to_string()),
            mime: spec.mime.clone().unwrap_or_else(|| mime.to_string()),
            name,
            iris: false,
        })
    }
}

/// Write through a temp file and rename, as `download_attachment` does.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

// ---------------------------------------------------------------------------
// event pump
// ---------------------------------------------------------------------------

/// Feed every APNs message through the client and publish what comes out.
///
/// `IMClient::handle` returns `Ok(None)` constantly — most APNs traffic is
/// keepalives and protocol chatter for other topics — so only real messages
/// are forwarded.
async fn pump(session: Arc<Session>, conn: rustpush::APSConnection) {
    let mut rx = conn.subscribe().await;
    loop {
        let aps = match rx.recv().await {
            Ok(m) => m,
            // Lagging means we fell behind a burst, not that we're done; the
            // alternative is exiting and losing the connection entirely.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("dropped {n} APNs messages while catching up");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Apple resets the APNs socket from time to time (seen as
                // "Connection reset by peer" from rustpush::aps). When the
                // broadcast channel closes with it, this pump is finished —
                // and simply returning left the helper *alive and serving*
                // with no connection behind it: sends still worked, the unix
                // socket stayed up, `IMessageStatus` still reported
                // available, and nothing inbound ever arrived again. Messages
                // sent from the iPhone stopped appearing on the desktop for
                // hours with nothing in the daemon's log to say why.
                //
                // Exit instead. The connection cannot be rebuilt in place —
                // it is owned by the client that was constructed around it at
                // startup — and the unit is Restart=always, so systemd brings
                // us back with a fresh one. The daemon sees the socket close,
                // drops the transport, and reconnects on its own retry timer.
                log::error!("APNs connection closed — exiting so the unit restarts us");
                let _ = session
                    .events
                    .send(json!({"event": "disconnected"}).to_string());
                // Long enough for the writer tasks to flush that event to any
                // connected client; the socket closing is the real signal, so
                // this is a courtesy rather than a guarantee.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                std::process::exit(1);
            }
        };
        if let Some(f) = &session.focus {
            f.handle(aps.clone(), session.events.clone());
        }
        match session.client.handle(aps).await {
            Ok(Some(inst)) => {
                if session.delivery_receipts {
                    // Off the pump: the receipt is a network round trip and
                    // must not hold up the next inbound message.
                    let s = session.clone();
                    let copy = inst.clone();
                    tokio::spawn(async move { s.acknowledge(copy).await });
                }
                // Before publishing, so a status query prompted by the
                // event already sees the adopted profile.
                session.maybe_adopt_profile(&inst).await;
                publish(&session, inst)
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("could not parse an inbound message: {e}");
                let _ = session
                    .events
                    .send(json!({"event": "parse_error", "error": e.to_string()}).to_string());
            }
        }
    }
}

fn publish(session: &Session, inst: MessageInst) {
    // Remembered before the event goes out, so the download the daemon starts
    // on receiving it can't lose a race with this.
    let atts = attachments_of(&inst);
    if !atts.is_empty() {
        if let Ok(mut cache) = session.attachments.try_lock() {
            cache.insert(inst.id.clone(), atts);
        } else {
            log::warn!("attachment cache busy; {} may not be fetchable", inst.id);
        }
    }
    match serde_json::to_value(&inst) {
        Ok(inst_json) => {
            let line = json!({"event": "message", "inst": inst_json}).to_string();
            // An error here only means nobody is listening yet.
            let _ = session.events.send(line);
            // Derived events follow the raw one; see poll.rs.
            for ev in poll::events_for(&inst) {
                log::info!("poll event: {ev}");
                let _ = session.events.send(ev.to_string());
            }
            if let Some(ev) = link::event_for(&inst) {
                log::info!("link event on {}", inst.id);
                let _ = session.events.send(ev.to_string());
            }
            if let Some(ev) = background::event_for(&inst) {
                log::info!("background event: {ev}");
                let _ = session.events.send(ev.to_string());
            }
        }
        Err(e) => log::warn!("could not serialise message {}: {e}", inst.id),
    }
}

// ---------------------------------------------------------------------------
// socket
// ---------------------------------------------------------------------------

/// Where the helper listens. Unix socket on Linux (unchanged); a loopback TCP
/// port with `--tcp`, which is the only option on Windows.
enum Listener {
    #[cfg(unix)]
    Unix(UnixListener),
    Tcp(TcpListener),
}

async fn serve(session: Arc<Session>, listener: Listener) {
    match listener {
        #[cfg(unix)]
        Listener::Unix(l) => loop {
            match l.accept().await {
                Ok((stream, _)) => spawn_client(session.clone(), stream),
                Err(e) => {
                    log::error!("accept failed: {e}");
                    return;
                }
            }
        },
        Listener::Tcp(l) => loop {
            match l.accept().await {
                Ok((stream, peer)) => {
                    // Bound to loopback, but any local process can still
                    // connect; there is no peer-credential check on TCP.
                    if !peer.ip().is_loopback() {
                        log::warn!("refusing non-loopback peer {peer}");
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    spawn_client(session.clone(), stream)
                }
                Err(e) => {
                    log::error!("accept failed: {e}");
                    return;
                }
            }
        },
    }
}

fn spawn_client<S>(session: Arc<Session>, stream: S)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = handle_client(session, stream).await {
            log::debug!("client disconnected: {e}");
        }
    });
}

async fn handle_client<S>(session: Arc<Session>, stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    // Push events to this client for as long as it stays connected.
    let mut events = session.events.subscribe();
    let (mut out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });

    let event_tx = out_tx.clone();
    let events_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(line) => {
                    if event_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // The request id is echoed even when the body is unparseable, so a
        // caller waiting on a specific id never hangs.
        let id = serde_json::from_str::<Json>(&line)
            .ok()
            .and_then(|v| v.get("id").and_then(|i| i.as_str().map(str::to_string)));

        let reply = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => match session.dispatch(id.clone(), cmd).await {
                Ok(data) => Reply { id, ok: true, error: None, data },
                Err(e) => Reply {
                    id,
                    ok: false,
                    // The chain matters here: "sending to Apple: …" alone
                    // doesn't say which of a dozen causes it was.
                    error: Some(format!("{e:#}")),
                    data: Json::Null,
                },
            },
            Err(e) => Reply {
                id,
                ok: false,
                error: Some(format!("bad request: {e}")),
                data: Json::Null,
            },
        };
        if out_tx
            .send(serde_json::to_string(&reply)?)
            .await
            .is_err()
        {
            break;
        }
    }

    events_task.abort();
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

// ---------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------

struct Args {
    state_dir: PathBuf,
    /// Not required with `--import-only`, which never binds.
    socket: Option<PathBuf>,
    import_from: Option<PathBuf>,
    /// Copy the registration in and exit. This is the monthly renewal path:
    /// OpenBubbles re-registers, we re-import, the service restarts.
    import_only: bool,
    /// Send as this handle rather than the default. Full form, e.g.
    /// `tel:+12155550100` or `mailto:you@icloud.com`.
    send_as: Option<String>,
    /// Listen on 127.0.0.1:PORT instead of a unix socket. Required on
    /// Windows; optional elsewhere.
    tcp_port: Option<u16>,
    /// When to attach our shared Name and Photo: always | first | never.
    share_profile: profile::ShareMode,
    delivery_receipts: bool,
    profile_name: String,
    /// Receive shared Focus status (read-only); see focus.rs.
    focus: bool,
}

fn parse_args() -> Result<Args> {
    let mut state_dir = None;
    let mut socket = None;
    let mut import_from = None;
    let mut import_only = false;
    let mut send_as = None;
    let mut tcp_port: Option<u16> = None;
    let mut share_profile = profile::ShareMode::Never;
    let mut delivery_receipts = true;
    let mut profile_name = "Group G".to_string();
    let mut focus = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => state_dir = args.next().map(PathBuf::from),
            "--socket" => socket = args.next().map(PathBuf::from),
            "--import-from" => import_from = args.next().map(PathBuf::from),
            "--import-only" => import_only = true,
            "--send-as" => send_as = args.next(),
            "--tcp" => {
                tcp_port = Some(
                    args.next()
                        .context("--tcp needs a port")?
                        .parse()
                        .context("--tcp port must be 1-65535")?,
                )
            }
            "--no-delivery-receipts" => delivery_receipts = false,
            "--focus" => focus = true,
            "--profile-name" => {
                profile_name = args.next().context("--profile-name needs a value")?
            }
            "--share-profile" => {
                share_profile =
                    profile::ShareMode::parse(&args.next().context("--share-profile needs a value")?)?
            }
            "--help" | "-h" => {
                println!(
                    "ib-imessage --state-dir DIR --socket PATH \
                     [--import-from OPENBUBBLES_DIR] [--import-only] \
                     [--send-as HANDLE] [--tcp PORT] \
                     [--share-profile always|first|never] \
                     [--no-delivery-receipts] [--profile-name NAME] [--focus]\n\n\
                     --profile-name labels a profile adopted from our own \
                     device in status (default \"Group G\").\n\n\
                     --no-delivery-receipts stops the helper acknowledging \
                     incoming messages, so senders never see Delivered.\n\n\
                     --share-profile attaches the Name and Photo in \
                     STATE_DIR/share_profile.json to outgoing messages: on \
                     every one, on the first per conversation since start, \
                     or never (default).\n\n\
                     --send-as picks the identity messages are sent from, \
                     e.g. tel:+12155550100. Defaults to the registered phone \
                     number, because sending from an email address starts a \
                     second conversation on the recipient's phone.\n\n\
                     --import-only copies the registration and exits without \
                     connecting; use it to adopt a renewed registration.\n\n\
                     {SINGLETON_NOTE}"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    if import_only && import_from.is_none() {
        bail!("--import-only needs --import-from");
    }
    Ok(Args {
        state_dir: state_dir.context("--state-dir is required")?,
        // Only the serving path needs somewhere to listen.
        socket: match (socket, import_only || tcp_port.is_some()) {
            (Some(s), _) => Some(s),
            (None, true) => None,
            (None, false) => bail!("--socket or --tcp is required"),
        },
        import_from,
        import_only,
        send_as,
        tcp_port,
        share_profile,
        delivery_receipts,
        profile_name,
        focus,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();
    let args = parse_args()?;

    let dir = StateDir::new(&args.state_dir);
    if let Some(src) = &args.import_from {
        dir.import_from(src)
            .with_context(|| format!("importing registration from {}", src.display()))?;
        log::info!("imported registration from {}", src.display());
        log::info!("{}", profile::import(src, &args.state_dir));
        log::info!("{}", profile::import_anisette(src, &args.state_dir));
    }
    if args.import_only {
        // Load once before declaring success: a copied-but-unloadable
        // registration would otherwise only fail at the next service start,
        // long after the person doing the renewal has walked away.
        let loaded = dir.load().context("verifying the imported registration")?;
        let handles: usize = loaded
            .users
            .iter()
            .filter_map(|u| u.registration.get("com.apple.madrid"))
            .map(|r| r.handles.len())
            .sum();
        log::info!("registration verified: {handles} handle(s) registered");
        return Ok(());
    }
    if !dir.is_seeded() {
        bail!(
            "{} has no registration; run once with --import-from \
             ~/.var/app/app.openbubbles.OpenBubbles/data/bluebubbles",
            args.state_dir.display()
        );
    }

    let loaded = dir.load().context("loading registration")?;

    // Surface staleness at startup rather than as a send failure later.
    let mut warnings = Vec::new();
    let ages = registration_age(&loaded.users);
    for (service, age) in &ages {
        if *age > REGISTRATION_WARN_SECS {
            warnings.push(format!(
                "{service} was registered {} days ago and will expire; open \
                 OpenBubbles to renew, then re-import",
                age / 86_400
            ));
        }
    }
    for w in &warnings {
        log::warn!("{w}");
    }

    let os_config = Arc::new(loaded.os_config);
    let (conn, err) = APSConnectionResource::new(os_config.clone(), Some(loaded.aps)).await;
    if let Some(e) = err {
        // Not fatal on its own: rustpush reconnects, and a transient DNS or
        // network failure at startup shouldn't stop the helper from coming up.
        log::warn!("APNs connect reported: {e}");
    }

    // `--focus` needs multiplex1 in the identity manager, and only if that
    // will not trigger a re-registration this build cannot perform.
    let (focus_on, focus_note) = if !args.focus {
        (false, "off (start with --focus)".to_string())
    } else {
        match focus::registration_ok(&loaded.users) {
            Ok(()) => (true, "on".to_string()),
            Err(why) => {
                log::warn!("--focus refused: {why}");
                (false, format!("refused: {why}"))
            }
        }
    };
    // rustpush's IDSService type is not exported, so let inference name it;
    // both arrays are constant expressions and are promoted to 'static.
    let services = if focus_on {
        &[&MADRID_SERVICE, &rustpush::findmy::MULTIPLEX_SERVICE] as &[_]
    } else {
        &[&MADRID_SERVICE] as &[_]
    };

    let save_dir = StateDir::new(&args.state_dir);
    let client = IMClient::new(
        conn.clone(),
        loaded.users.clone(),
        loaded.identity,
        services,
        loaded.dir.join("id_cache.plist"),
        os_config.clone(),
        Box::new(move |users| {
            // rustpush hands back refreshed IDS keys; losing these means the
            // next start re-registers, which this build cannot do.
            if let Err(e) = save_dir.save_users(&users) {
                log::error!("could not persist refreshed IDS keys: {e}");
            }
        }),
    )
    .await;

    let handles: Vec<String> = loaded
        .users
        .iter()
        .flat_map(|u| {
            u.registration
                .get("com.apple.madrid")
                .map(|r| r.handles.clone())
                .unwrap_or_default()
        })
        .collect();
    if handles.is_empty() {
        bail!(
            "no handles registered for com.apple.madrid — OpenBubbles has not \
             finished registering, or the registration lapsed"
        );
    }
    log::info!("registered handles: {}", handles.join(", "));

    // A bad profile file is a warning, not a startup failure: sending without
    // the profile is what happened before it existed.
    let shared_profile = match profile::load(&args.state_dir) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("shared profile not loaded: {e:#}");
            None
        }
    };
    match (&shared_profile, args.share_profile) {
        (Some(p), m) => log::info!(
            "shared profile {} ({}); --share-profile {}",
            p.message.cloud_kit_record_key,
            p.display_name.as_deref().unwrap_or("no display name"),
            m.as_str()
        ),
        (None, profile::ShareMode::Never) => {}
        (None, m) => log::warn!(
            "--share-profile {} but no {} in the state dir; sending without it",
            m.as_str(),
            profile::FILE
        ),
    }

    let focus = if focus_on {
        match focus::Focus::start(conn.clone(), os_config.clone(), client.identity.clone(), &args.state_dir).await {
            Ok(f) => Some(f),
            Err(e) => {
                log::warn!("Focus receiver did not start: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let focus_note = if focus_on && focus.is_none() { "failed to start; see log".to_string() } else { focus_note };

    let (events, _) = broadcast::channel(1024);
    let session = Arc::new(Session {
        client,
        conn: conn.clone(),
        attachments: Mutex::new(AttachmentCache::default()),
        handles,
        events,
        send_lock: Mutex::new(()),
        warnings,
        send_as: args.send_as.clone(),
        profile: Mutex::new(shared_profile),
        profile_auth: Mutex::new(setprofile::Auth::default()),
        state_dir: args.state_dir.clone(),
        os_config: os_config.clone(),
        share_mode: args.share_profile,
        delivery_receipts: args.delivery_receipts,
        profile_name: args.profile_name.clone(),
        profile_sent: Mutex::new(profile::Sent::default()),
        focus,
        focus_note,
    });
    // Worth a line of its own: if this is an email address, every message
    // will open a second thread beside the one the recipient's phone already
    // has with your number.
    match session.my_handle() {
        Ok(h) => log::info!("sending as {h}"),
        Err(e) => log::warn!("no sending identity: {e}"),
    }

    let listener = match args.tcp_port {
        Some(port) => {
            // Loopback only. The socket sends as the user's Apple ID; it must
            // never be reachable from the network.
            let l = TcpListener::bind(("127.0.0.1", port))
                .await
                .with_context(|| format!("binding 127.0.0.1:{port}"))?;
            log::info!("listening on 127.0.0.1:{port}");
            Listener::Tcp(l)
        }
        None => bind_unix(args.socket.as_ref().expect("checked in parse_args"))?,
    };

    tokio::spawn(pump(session.clone(), conn));
    serve(session, listener).await;
    Ok(())
}

#[cfg(not(unix))]
fn bind_unix(_path: &std::path::Path) -> Result<Listener> {
    bail!("unix sockets are not supported on this platform; use --tcp PORT")
}

#[cfg(unix)]
fn bind_unix(socket_path: &std::path::Path) -> Result<Listener> {
    // A unix socket's inode outlives the process that bound it, so a crash or
    // a SIGKILL leaves a file that makes bind() fail with EADDRINUSE forever.
    // Unlink unconditionally rather than testing exists() first: the check is
    // racy, and a removal failure must be reported, not swallowed.
    match std::fs::remove_file(socket_path) {
        Ok(()) => log::debug!("removed stale socket {}", socket_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "could not remove stale socket {}",
                socket_path.display()
            )))
        }
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    {
        // The socket sends messages as the user's Apple ID; nobody else on the
        // machine should be able to.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    log::info!("listening on {}", socket_path.display());
    Ok(Listener::Unix(listener))
}

// Silence an unused-import warning when the map type is only used indirectly.
#[allow(dead_code)]
type _Ages = HashMap<String, u64>;

#[cfg(test)]
mod wire_tests {
    use super::Command;

    fn parse(s: &str) -> Command {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn new_commands_parse() {
        assert!(matches!(parse(r#"{"cmd":"set_profile","name":"Group G","first":"Group","last":"G","photo":"C:\\x.jpg"}"#), Command::SetProfile { .. }));
        assert!(matches!(parse(r#"{"cmd":"set_profile","name":"Group G","tfa_code":"123456"}"#), Command::SetProfile { tfa_code: Some(_), .. }));
        assert!(matches!(parse(r#"{"cmd":"read_receipt","chat":{"participants":["tel:+1"]},"guid":"ABC"}"#), Command::ReadReceipt { .. }));
        assert!(matches!(parse(r#"{"cmd":"mark_read","chat":{"participants":["tel:+1"]}}"#), Command::MarkRead { guid: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"mark_read","chat":{"participants":["tel:+1"]},"guid":"ABC"}"#), Command::MarkRead { guid: Some(_), .. }));
        assert!(matches!(parse(r#"{"cmd":"set_icon","chat":{"participants":["tel:+1"],"guid":"G"}}"#), Command::SetIcon { path: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"send","chat":{"participants":["tel:+1"]},"attachments":[{"path":"C:\\a.pdf"}]}"#), Command::Send { .. }));
        assert!(matches!(parse(r#"{"cmd":"send_poll","chat":{"participants":["tel:+1"]},"question":"Q","options":["a","b"]}"#), Command::SendPoll { .. }));
        assert!(matches!(parse(r#"{"cmd":"poll_vote","chat":{"participants":["tel:+1"]},"poll_guid":"G","option_ids":[]}"#), Command::PollVote { session: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"send","chat":{"participants":["tel:+1"]},"text":"hi","format":[{"start":0,"len":2,"bold":true}],"mentions":[{"start":0,"len":2,"handle":"tel:+1"}],"as_audio_message":false}"#), Command::Send { .. }));
        assert!(matches!(parse(r#"{"cmd":"send_link","chat":{"participants":["tel:+1"]},"url":"https://x.com"}"#), Command::SendLink { fetch: true, title: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"send_link","chat":{"participants":["tel:+1"]},"url":"https://x.com","title":"T","summary":"S","image_path":"C:\\i.png","fetch":false}"#), Command::SendLink { fetch: false, image_path: Some(_), .. }));
        assert!(matches!(parse(r#"{"cmd":"download_link_image","guid":"G","dest":"C:\\o.jpg"}"#), Command::DownloadLinkImage { .. }));
        assert!(matches!(parse(r#"{"cmd":"focus_status","handles":["tel:+1"]}"#), Command::FocusStatus { .. }));
        assert!(matches!(parse(r#"{"cmd":"focus_status"}"#), Command::FocusStatus { .. }));
        assert!(matches!(parse(r#"{"cmd":"set_background","chat":{"participants":["tel:+1"],"guid":"G"}}"#), Command::SetBackground { path: None, version: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"set_background","chat":{"participants":["tel:+1"]},"path":"C:\\b.zip","version":3}"#), Command::SetBackground { path: Some(_), version: Some(3), .. }));
        assert!(matches!(parse(r#"{"cmd":"poll_add_option","chat":{"participants":["tel:+1"]},"poll_guid":"G","text":"Fri"}"#), Command::PollAddOption { session: None, .. }));
        assert!(matches!(parse(r#"{"cmd":"download_background","guid":"G","dest":"C:\\b.zip"}"#), Command::DownloadBackground { .. }));
        assert!(matches!(parse(r#"{"cmd":"set_background","chat":{"participants":["tel:+1"]},"image_path":"C:\\p.heic"}"#), Command::SetBackground { image_path: Some(_), .. }));
        assert!(matches!(parse(r##"{"cmd":"set_background","chat":{"participants":["tel:+1"]},"gradient":{"colors":["#DCD2F2",[0.2,0.7,0.9]]}}"##), Command::SetBackground { gradient: Some(_), .. }));
        assert!(matches!(parse(r#"{"cmd":"set_background","chat":{"participants":["tel:+1"]},"dynamic":"ocean_2"}"#), Command::SetBackground { dynamic: Some(_), .. }));
    }
}
