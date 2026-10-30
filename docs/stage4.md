# Stage 4 Design Draft: IR Connectors and the Own IR on Cloudflare

Stage 4 of the roadmap (design.md "Roadmap") is the own IR: server-side computation, start registration, impossible-input detection and the allowlist. Stage 6 connects to existing IRs. This draft decides both ahead of time, together, so that one connector contract in the player serves the own IR and existing IRs alike. The first players are expected to come from the BMS players in use today, so the IRs they use and the history they bring set the contract.

Nothing here is built before stage 3 is done (design.md "Drawing the line on scope"). The only work that may land earlier is what design.md already requires of the client side, such as keeping the WASM build of `eval` passing in CI.

**Status.** Draft. Decisions marked *(owner)* change public behavior, cost or licensing and wait for the owner. Every other decision is a method choice. This file is removed when stage 4 exits, once its decisions are in design.md.

**Direction applied throughout:**
- **Small cores with named extension surfaces:**
  - In the player, every IR is reached through a connector behind one contract. The own IR gets no special path.
  - On the server, a new preset, metric, ranking or eligibility rule is added without changing the core.
- **Input logs are the source of truth for every score:**
  - The server's derived data can be rebuilt from them.
  - Its operational records cannot: accounts, admissions, receipt times, visibility, verification and deletion. They are kept and backed up as records.
- **Cloudflare limits are met by structure** (reservations before spending, claimed jobs, staged checks), not by tuning one path at a time.
- **Nothing is built ahead of its first consumer.** A capability can be named in the contract before anything implements it.

## Part A. The player side

### A0. The IRs players use today (October 2026)

| IR | Players that connect | Open to a new client? | Chart key | Payload |
| --- | --- | --- | --- | --- |
| Mocha-Repository | beatoraja (bundled plugin) | No. Modified builds cannot log in, and its terms forbid reverse-engineering the API | SHA-256 | Score |
| MinIR | beatoraja (closed-source plugin) | No public API | SHA-256 | Score |
| BMS-IR (LR2IR's unofficial successor) | LR2 (connection patch), beatoraja family (plugin), bmz-player (built in) | Per-client API namespaces, so it needs the operator's cooperation | MD5 | Score |
| rianIR | beatoraja, LR2oraja, bmz-player | Needs the operator: only registered executable hashes may submit | SHA-256 | Score, signed |
| Bokutachi (Tachi) | LR2oraja Endless Dream, OpenLR2 (DLL), RhythmGame (forwarded by its server) | Partly. Server and plugin are open source, and a manual import route takes an API key. The beatoraja route accepts only expected client names | MD5 or SHA-256 | Score |
| BMZ IR | bmz-player only | Documented REST API with email sign-in; its terms are not yet checked | SHA-256 (MD5 kept) | Score, replay stored by hash |
| RhythmGame IR | RhythmGame only | Open-source server | — | Score with replay |
| LR2IR | — | Closed 31 May 2026; a read-only archive remains | MD5 | — |

**How the players integrate:**
- **beatoraja:** a plugin interface (`IRConnection`). Since 0.8.9, each plugin runs in its own process over JSON lines on stdio.
- **OpenLR2:** a DLL ABI.
- **bmz-player:** built-in providers, plus custom URLs that speak a known protocol.
- **Lunatic Vibes F:** has no IR client.

**What they share:**
- **Writes and reads:** they send to every configured IR, and read rankings, rivals and tables from one primary IR.
- **Send policies:** always, complete song, or improved score.
- **Payload:** a score summary (judge counts with fast/slow, clear, combo, BP, options and seed, gauge, LN mode, rule and device type), with both MD5 and SHA-256 at hand.
- **Rules per IR:** each IR enforces its own option, LN and `#RANDOM` rules and reports a refusal in its answer.
- **Resending:** only bmz-player keeps a durable send queue with idempotency keys. The others resend from memory.

**What follows for tapestry:**
- No existing IR can be reached without either its operator accepting tapestry as a client, or a connector the operator or community writes. The contract must make writing a connector cheap, in any language.
- The players being left behind store their history in SQLite: beatoraja and its forks in `score.db`/`scorelog.db`, LR2 and Lunatic Vibes F in their score databases, bmz-player in its `score.db`. Importing that history matters as much as sending new scores (C7).

### C1. One connector contract, out of process

An IR is reached through a **connector**: a separate process the player starts, speaking versioned JSON messages one per line over stdio. A connector implements one **protocol**. The player configures **instances** of it, each with its own destination (base URL) and account. So two IRs that speak one protocol need one connector, as with bmz-player's custom URLs.

The player side is the **IR host**. It plugs into the saving extension point (`RecordSink`, `crates/game/src/save.rs:60`), into play start (C4, start grants), and into the result and song-select screens.

- **Why out of process:**
  - A connector is written without rebuilding the player, by the IR's operator or anyone else.
  - A crash or hang cannot take the game with it.
  - It is the shape beatoraja moved to in 0.8.9.
- **Built-in connector:** the own IR's connector ships with the player as a subcommand of the same executable, and goes through the same protocol. The contract is therefore exercised by a connector we control.
- **Process lifetime:**
  - One long-lived process per configured instance, with one request in flight at a time.
  - After a timeout, a malformed or oversize frame, or end of output, the host kills and reaps the process before it restarts it and repeats the handshake. A late answer to a timed-out request can therefore never be read as the answer to the next one.
  - Frames, queued output and stderr have byte bounds.
- **Trust:**
  - Connectors are trusted native programs the player chose to install, like any application.
  - Process separation contains crashes. It does not isolate secrets from a malicious connector.
- **Licensing:**
  - The bundled connector is GPL like the player.
  - Whether independently written connectors that use only the published protocol may carry any license is *(owner)*. If yes, it is stated in `COPYRIGHT` next to the local event endpoint's statement ("Licenses"), not assumed from process separation.
- **Alternatives:**
  - An in-process Rust trait, like bmz-player's built-in providers. Every IR would need a release of the player, and there would be no crash isolation.
  - A DLL ABI, like OpenLR2. Its ABI is tied to one compiler, and it runs in-process.
  - WASM components in an embedded runtime. These would sandbox secrets as well, but bring a large runtime, and network access for components is still young. Revisited if connectors from unknown authors become common.

### C2. The handshake: needs and capabilities

The host and the connector exchange `hello` messages that carry protocol versions and the capabilities each side knows. The connector's manifest declares its needs, and the host sends only what the manifest asks for.

| Field | Values |
| --- | --- |
| Protocol | Version. The host refuses one it does not speak |
| Identity | Protocol name, connector version, home URL. Instance destination and account come from the host's configuration |
| Payload | `score` (a summary) or `record` (the input log bytes, plus `chart_text` when the destination asks for it) |
| Chart identity | `md5`, `sha256` or both |
| Accepted evaluation | Canonical preset hashes (never a bare standard preset id, since its definition can change) and the LN judgment modes |
| Accepted play | The play types and options the IR accepts, so the host does not send what would be refused |
| Capabilities | `start_grants`, `browser_sign_in`, `rankings`. Named for later: `rivals`, `tables`, `refused_charts`, `courses`, `incremental_sync` |

- **Unknown capabilities:** the host ignores an unknown capability. A connector may not require a capability the host did not list.
- **New capabilities:** a new one arrives with a protocol minor version.
- **Later capabilities:** those named for later are only reserved names. The host implements each when its first connector needs it.

### C3. Neutral vocabulary; connectors translate

The host sends types defined by the protocol crate:

- **`Chart`:** MD5, SHA-256, the resolved hash, and metadata.
- **`Provenance`:** the facts of the play as played:
  - the canonical preset hash;
  - the evaluator version;
  - the gauge the session policy played;
  - the play type, the chart transform's id, rule version and seed;
  - speed, assists and the LN mode in effect;
  - each source's device class (keyboard, controller, MIDI).
- **`ScoreSummary`:** judge counts with fast/slow, EX score, max combo, BP and notes, plus per-gauge results with their histories.

Each connector maps these to its IR's codes: clear-type and option enumerations, and rule and device names. An IR's vocabulary never enters the host, so an IR with other codes is a connector change only.

- **Integers:** values beyond 2^53, such as seeds, are JSON strings, so a connector in any language reads them losslessly.
- **Which plays go out** *(owner)*. Proposed:
  - Only plays **played** under an accepted preset are sent, matched by preset hash from `Provenance` before a summary is made.
  - A what-if re-evaluation under another preset is never sent to an external IR, because the player did not see that gauge and the IR cannot tell.
  - The own IR receives the record itself, and evaluates it under every preset on the server.

### C4. Host services

Every connector gets the same services, so a connector stays a thin translator.

- **Outbox:**
  - **Durable before sending:** a send is written to a per-instance outbox under the data root before its first attempt.
  - **Bound to its account:** each item is bound to the instance and the remote account that produced it. Signing in to another account or changing the destination never retargets pending items; they wait for that account or are discarded by the player.
  - **Answers:** the connector answers `ok`, `retry` (with an optional delay) or `refused` (with a reason shown to the player). The host retries with backoff across restarts.
  - **Idempotency:** each item carries an idempotency key, made from the record's hash and the instance id. Delivery is at least once: an IR that ignores the key can receive a duplicate after a lost answer.
  - **Send policy:** per instance, `always`, `complete_song` or `update_score`.
- **Primary instance:** one primary instance answers reads (rankings first), and every enabled instance receives sends.
- **Chart text:** for a connector that asks for `chart_text`, the host keeps the exact bytes of the recorded chart hash with the pending item. It reads them while the file still matches, so a changed or removed file does not lose them.
- **Secrets:** stored in the Windows credential store under the instance id and remote account, never under a display name. The host hands an instance only its own.
- **Browser sign-in:**
  - The host owns the transaction. It binds an exclusive loopback listener first, then builds the authorization URL with `state` and PKCE ([RFC 8252](https://datatracker.ietf.org/doc/html/rfc8252)).
  - It accepts one callback that matches, then closes the listener.
  - The own IR's redirect carries a short-lived redemption code. The connector exchanges that code for the credential, so no credential appears in a URL.
  - Password IRs use a form the host draws from a field list in the manifest.
- **Start grants** (`start_grants`):
  - A grant is `(seed, expiry, opaque proof)`. Connectors fetch them in batches ahead of play.
  - At play start, before the chart is resolved, the host takes the next grant of the **one** instance chosen as the seed authority for that play, and uses its seed for every random choice.
  - Proofs are stored in the record outside the part a published replay shows.
  - With no grant available (offline, or the pool is used up), the play uses a local seed. Its record carries no proof and is "unverified" (design.md "Rerolling RANDOM is normal play").
- **Placement:**
  - No connector call happens during play; the grant is taken from the local pool.
  - Connectors run after the result, in song selection, and in settings.
  - A failure is reported and the game continues (design.md "Extension Boundaries").
  - Reads come back as neutral types, and song selection draws from the cache only.

### C5. Crates: the SDK

| Crate | Layer and license | Role |
| --- | --- | --- |
| `ir-protocol` | L2, `MIT OR Apache-2.0` | Messages, the manifest, the neutral vocabulary, and the line codec. It is the SDK for a connector in Rust; other languages use its JSON schema, generated from it and checked in |
| `ir-host` | L4, GPL | Processes, handshake, outbox, grants, timeouts. Portable, tested on Linux and macOS with fake connectors. The credential store sits behind a trait, and the Windows implementation is in `player` |
| own IR connector | GPL, a subcommand of `player` | The own IR's HTTP API: browser sign-in, grants, record and chart text upload |

**The first external connector** is chosen in stage 6 among the IRs whose operator accepts tapestry, truthfully named, as a client. Checking the terms is not enough. For Tachi, its beatoraja route currently accepts only expected client names, so the first step is the operator's acceptance of the route and of tapestry's evaluation semantics. A successful translation alone proves nothing.

### C6. Testing the contract

- **Fake connector:** a fake connector, scripted by tests, drives the host through:
  - the handshake, and every answer;
  - a crash;
  - a timeout followed by a late answer;
  - an oversize unterminated frame;
  - an unknown capability;
  - a restart with pending sends;
  - a change of account with pending sends.
- **Own connector:** tested against the server running under workerd (Part B, S4).
- **Shared fixtures:** message fixtures from `ir-protocol` are shared with the server's tests, so a change on either side that breaks the other fails CI.

### C7. History import (a separate extension, related)

Bringing history over from other players is not an IR feature, but it decides whether players move.

- **What is imported:** an **import** extension reads another player's score database and writes **imported bests**. They are marked by source and kept apart from tapestry's records.
- **What can be imported:** each import declares what its source lacks, the way bmz-player's import does. LR2 rows have no fast/slow split, and imported rows have no play time.
- **Replays:** another player's replay is not converted into an input log. It has no device profile or receive times, and evaluating it would claim a precision it does not have.
- **Uploading:** imported bests never go to the own IR as scores (design.md "Anti-Cheating Measures"). Whether they may be shown on a profile is a later decision.

## Part B. The server: the own IR on Cloudflare

### Shape

```
connector ──POST /v1/records──▶ Admit ──▶ Store ──▶ (queue nudge)
                                                        │
                    Evaluate ──▶ Measure ──▶ Classify ──▶ Project
                    (WASM eval)  (metrics)   (eligibility, (boards)
                                              detectors)
```

| Role | What is used |
| --- | --- |
| API and pipeline | Workers, TypeScript |
| Evaluator, metrics, detectors | WASM modules built from Rust (`ir-wasm`, a new permissive crate over `eval`, `bms-parser` and `replay`), called from the queue consumer |
| Record bytes and chart text | R2, content-addressed and never rewritten |
| Operational records | D1: accounts, credentials, start grants, admission reservations, submissions with server receipt time, visibility, verification, deletion |
| Derived data | D1: evaluations, metric values, facts, detector results, boards. Rebuildable from R2 and the operational records |
| Coordination | Queues as nudges only. A cron reclaims and schedules work |
| Abuse limits | Rate Limiting binding per IP as a first line. Per-account and global allowances are D1 reservations |

**What sets this server apart.** bmz-player's own IR, also on D1 and R2, stores replays by hash and never evaluates them on the server. This server evaluates every record, so most of what follows bounds the cost and the correctness of that evaluation.

### Core and extension surfaces

The core is what must be correct for every record, and what a third party could not be trusted to get right:

- **Admit:** authentication, grants, reservations and size limits.
- **Store:** content-addressed record storage.
- **Evaluate:** running the pure evaluator.
- **Producer keys:** the keys of every derived row.
- **Jobs:** the runner that every stage after Store uses.

| Surface | Kind | Contract | Added by |
| --- | --- | --- | --- |
| Presets | Data | A canonical preset encoding (design.md "Rule Presets"), registered by hash | A registry row and its file |
| Metrics | Transform | A pure Rust function `JudgeEvent stream → i64`, fixed once registered by id and version. Its order and tie-break are declared with it | Code in `ir-wasm`, reviewed in the repo |
| Eligibility rules | Gate | A conjunction over a fixed set of facts the core computes: play type, speed, assists, offsets within limits, chart on the allowlist, LN mode. Not a rule language | A rule row |
| Allowlist sources | Inject | Each source (difficulty table, registry, manual entry) writes chart hashes with provenance into one allowlist | A source adapter, run as a cron job family |
| Detectors | Observe | A pure function over the record and its judgment stream, returning flags with a reason and a completion status. It cannot change an evaluation or a board | Code in `ir-wasm` |
| Trust policy | Gate | Maps detector results, account age and verification to what a board shows. A required detector that did not complete counts as unknown, never as clean | A policy row |
| Boards | Transform | Chart scope, preset hash, LN mode, metric id@version, eligibility rule, trust policy, and the per-player best | A board row |
| Sign-in providers | Handle | One OAuth adapter interface. Stage 4 builds only the provider it needs | An adapter module |
| Read API | Observe | A versioned read-only JSON API over boards and public records, for connectors and external tools | No server change |

- **Gates** fail closed.
- **Detectors** cannot block or change anything upstream. Their failure is recorded as their status.
- **Emit** (an outbox of board changes for webhooks and feeds) is a named surface with no implementation until something consumes it.

### Intentionally not in the core

| Capability | Why not in the core | Path |
| --- | --- | --- |
| Accepting scores | Scores are computed, never received (design.md "Anti-Cheating Measures") | Existing IRs receive scores from their connectors in the player |
| Uploaded code (user metrics, scripts) | Untrusted code on the operator's bill and against the rankings' trust | Metrics and detectors are reviewed code; presets, rules and boards are data |
| Automatic sanctions | Sophisticated automated input cannot be told apart (design.md "Anti-Cheating Measures") | Detectors flag, trust policy limits what is shown, a human decides |
| Audio, images, asset resolution | The server does not resolve what charts reference (design.md "Server Architecture") | None |
| Social features (follows, comments, chat) | Not needed for rankings; moderation cost | External tools over the Read API |
| Live push of boards | Cost, and long-lived connections for a page that changes on each submission | Polling with short cache lifetimes |
| Web replay viewer | The player plays published replays | An external tool over the Read API |
| Chart registry and installer | Stage 5 | An allowlist source |

### S1. Admission: reserve, store, finalize

1. **Cheap refusals.** The Worker refuses requests with no credential, an oversize `Content-Length`, or a per-IP limit hit, before touching R2, D1 or the queue. Body size is checked again while streaming, and every bound names its unit.
2. **Authenticate.** Look up the credential by its SHA-256. Then look up an existing submission bound to this account, grant and record hash. If there is one, return its receipt now, before any expiry or allowance check. This makes a retry after a lost acknowledgement succeed, without new jobs or allowance.
3. **Reserve.** One D1 batch:
   - checks the grant, if the record has one: its account, its expiry, and that it is unused. A record without a grant (offline play, C4) is admitted as unverified under the same allowances; only eligibility for verified boards requires a grant;
   - takes the per-account and global allowances;
   - inserts a **pending admission** with an expiry.

   Refusals happen here, before anything is spent on storage.
4. **Store.** Write the bytes to R2 at `records/<sha256>`. The write is idempotent.
5. **Finalize.** One D1 batch, conditioned on the pending admission still being pending and the account not deleted:
   - marks the grant used and binds it to the hash;
   - turns the reservation into a submission with the server receipt time;
   - inserts its jobs.
6. **Acknowledge** only after step 5.

**Abandoned admissions:**
- A daily job moves expired pending admissions to `abandoned`, with a conditional update.
- Deletion is decided per **object hash**, not per admission. An object is deleted only when no pending admission and no submission references its hash, and the hash is fenced against new reservations while the deletion runs.
- No R2 lifecycle rule deletes records.
- The implementation PR chooses the schema and the coordination.

**What a grant proves:**
- The record's seed must be the grant's seed.
- The guarantee is that the grant was issued, used once and unexpired, with its seed bound to the record. Prefetched grants do not prove when the input happened.

### S2. D1, not one SQLite Durable Object

The zenith and kopje projects keep all state in one SQLite Durable Object. That fits a single-operator service whose writes need synchronous multi-row transactions. The IR has a different shape:

- **Reads:** public reads across charts (boards, player pages, recent plays) far outnumber writes, and D1 serves them from read replicas through the Sessions API.
- **Writes:** the atomicity writes need comes from D1's transactional `batch()`; D1 rejects an explicit `BEGIN`.

bmz-player's IR made the same choice for the same reads.

- **Capacity:**
  - A D1 database is capped at 10 GB and has a single writer.
  - The estimate is made from the concrete schema and its indexes when they exist, with room for two overlapping generations (S6).
  - The watched indicators are size, writer latency and overload errors, and job backlog.
  - Allowances and the backfill rate are set from the measured writer work per submission, with room kept for admission.
  - Near a limit, evaluations and metric values split by chart hash across more databases.
- **Statement limits:** statements bind at most 90 parameters, under D1's 100, and longer lists are split.
- **Alternatives:**
  - One Durable Object per chart. Player pages and recent plays span charts, so they would need a second store.
  - One Durable Object for everything. A single writer with no replicas, which every read would wake.

### S3. Evaluation: WASM in the queue consumer

- **The module:** `ir-wasm` exports one function. It takes the record bytes, the chart text bytes, the preset encodings, and the metric and detector ids. It returns result bytes, a versioned canonical encoding in the style of the input log ("Input Log Format"): the tally, gauges, metric values, facts and detector results.
- **The boundary:**
  - Only bytes cross it, with `wasm-bindgen` as the glue.
  - Each job instantiates the compiled module anew.
  - The job drops the instance and its inputs **before** it awaits any write. Evaluation itself is synchronous, so one isolate never runs two evaluations at once.
  - Buffered reads of other invocations, and memory not yet collected, still count toward the isolate's peak. The measurement below covers them.
- **Bounded per invocation:**
  - A consumer invocation runs one evaluation job (`max_batch_size = 1`).
  - Its total CPU time and peak memory are measured on Workers, and kept within half of the configured CPU limit and half of 128 MB.
  - The cron runs no evaluation; it only reclaims expired claims and nudges the queue.
- **Staged checks:** each stage knows what it checks before it allocates.
  1. The compressed size and the declared uncompressed size, before decompressing.
  2. The decoded counts of events and sources, before the vectors that hold them.
  3. Chart loading under the server's loader limits, which are tighter than the client's.
  4. The event count against the loaded chart's length times a maximum input rate.
  5. Before evaluating, a budget computed from the known counts: notes, inputs and every preset's gauges. This budget is #97's resolution, which is decided before stage 4.
- **Over a limit:**
  - A job over a limit parks **that record**, with the reason.
  - A chart is marked over the server's limits only when its loading alone fails, which is recorded per loader version and resolution. Another player's valid record for the same chart is unaffected.
- **Loader without a filesystem:**
  - `load_bms` canonicalizes the chart folder (`crates/bms-parser/src/lib.rs:208`), which fails on `wasm32-unknown-unknown`.
  - Stage 4 adds an entry that loads from bytes and keeps sound names as written. The resolved-chart hash already excludes resolved paths ("Chart Model").
- **Versions:**
  - The server evaluates with its own evaluator, and treats the record's claimed evaluator version as information only (design.md "Threat Model"). It refuses record format versions it cannot decode.
  - `replay::verify` refuses a mismatched evaluator version, which is right on the client. The server calls the entry below that check.
- **Parity:**
  - A test-only output carries the resolved-chart hash and the ordered judgment event stream.
  - CI compares native execution with the module running in workerd (`@cloudflare/vitest-plugin`) on the same records, and stage 4 adds the same check against the deployed Workers (design.md "Server Architecture").
- **zstd:**
  - `replay` uses `zstd-sys`, which builds for `wasm32-unknown-unknown` with a clang that has the wasm target (checked on macOS).
  - If Linux CI cannot build it, the server decodes with `ruzstd`.
- **Measured so far** (V8 in Node on macOS, synthetic charts):
  - The evaluator runs at 1.15–1.3× native: 0.34 ms for 2,000 notes and 1.7 ms for 10,000.
  - Linear memory peaks at 4 MB and 15 MB respectively.
  - The module is 59 KB gzipped for the evaluator alone, and 292 KB with the loader.
  - Decompression and loading under workerd are not measured yet.
- **Alternative:** a container running the native build. A second runtime and per-instance cost for work that fits in WASM. Revisited if measured jobs leave the budget.

### S4. TypeScript for the Workers, in this repository

- **Toolchain:**
  - TypeScript, with the toolchain of the other Cloudflare projects: pnpm with a pinned catalog and a minimum release age, oxlint, oxfmt, and `@cloudflare/vitest-plugin` on workerd.
  - Security code uses established libraries, such as `jose` for any JWT.
  - Alternative: `workers-rs` throughout. Not chosen, because its bindings and test tooling lag, and only the evaluator needs Rust.
- **Location:**
  - The server lives under `server/`, and `ir-wasm` in the Cargo workspace.
  - This way an evaluator change, its parity check, and the protocol fixtures shared with the connector land in one PR.
- **Licenses:**
  - `ir-wasm` is `MIT OR Apache-2.0`.
  - The license of `server/` is *(owner)*.

### S5. Jobs: claimed before work, completed by the claim

The pattern follows the kopje project's job runner and bmz-player's send queue.

- **Jobs:**
  - A unit of work is a row: kind, key, state, attempts, claim id, claim expiry, next attempt time, last error.
  - A queue message only says "run a round".
- **Claiming:**
  - A runner claims a job before loading anything. One conditional `UPDATE … RETURNING` sets a new claim id and expiry and increments the attempts.
  - A job the platform kills has its attempt counted already. After a fixed number of attempts it is parked, so a poisoned record cannot be selected forever.
  - An expired claim is reclaimed.
- **Completing:** a job's results, its projection changes and its completion are written in **one** D1 batch, conditioned on its claim id and on its submission not being deleted. A runner whose claim expired, or whose record was deleted meanwhile, writes nothing.
- **Projections:**
  - A per-player best is a conditional upsert that replaces the row only when the new result is better.
  - Ties go to the earlier server receipt, then the lower submission id.
- **Failure boundaries:** each job family (evaluate, backfill, allowlist source) is its own boundary.
- **Tests:**
  - termination right after claiming;
  - two overlapping runners;
  - completion after the claim expired;
  - completion after deletion;
  - a best submitted twice in either order.

### S6. Producer keys, generations and backfills

- **Producer keys:** every derived row is keyed by everything that can change it:
  - the record and the preset hash;
  - the evaluator version and the loader version;
  - the chart transform's id and rule version;
  - the metric or detector id@version.
- **Generations:**
  - A board reads one **generation**: a fixed set of producer versions.
  - Its definition is immutable. Its contents change with submissions, allowlist removals, verification and deletions.
- **Executable generations:**
  - A generation stays open only while the deployed Worker carries the WASM modules of its producer versions. The modules are small (S3), so a deployment can carry two.
  - A generation whose modules are no longer deployed is frozen, and switching back to it is not offered.
- **Opening a generation:**
  - A change of producer versions opens a new generation as **building**.
  - From then on, every admission schedules work for every open generation. A backfill covers the submissions before it opened.
- **Switching:**
  - The active pointer switches in one batch, conditioned on the building generation having no unfinished work for any submission received up to that moment. Each such submission must be evaluated or explicitly excluded with a reason; a parked job blocks the switch until an operator resolves it.
  - Switching back to a still-open generation uses the same condition.
- **Reads:**
  - A cache fill starts its D1 session at `first-primary` and reads the pointer and the board in it.
  - Board pages are cached at the edge for a short lifetime, so a removal, a deletion or a change of visibility shows within that lifetime.
  - Only the active generation is served publicly.
- **Which change replaces a board and which starts a new one** *(owner)*, design.md's open question:
  - Proposed: an evaluator, loader or transform bug fix recomputes and switches.
  - Proposed: a changed preset or metric definition is a new board, and the old one is kept, frozen.

### S7. Accounts and credentials

- **Sign-in:** only through external OAuth providers, behind the adapter interface. Stage 4 builds the one it needs *(owner: which)*.
- **Credentials:**
  - A connector exchanges the sign-in's redemption code (C4) for an API credential: 32 random bytes, stored and looked up only by SHA-256, and scoped to submission, grants and reads.
  - If credentials expire, a refresh credential rotates. A reused refresh credential that was rotated more than a short grace period ago revokes the account's sessions, as in bmz-player's IR.
- **Account deletion:**
  - First, it revokes the credentials and marks the account deleted. Admission finalization and job completion are conditioned on that mark (S1, S5).
  - Then it removes the records from R2 and the rows from the operational and derived data.
  - The deletion marker is kept, so a rebuild never restores what was deleted.

### S8. Operation

- **Deployment:**
  - One deployment on the owner's account, on Workers Paid.
  - Public configuration is in `wrangler.jsonc`. Account values are in a gitignored deploy file, merged by a deploy script that refuses unknown keys.
- **Logs:** Workers Logs with invocation logs off. Only a structured logger writes, with a closed set of fields. No record contents and no credentials are logged.
- **Backups:**
  - R2 records are never rewritten.
  - D1 time travel covers recent mistakes.
  - The operational tables are exported to R2 daily with a manifest written last, because they cannot be rebuilt.
- **Threat model:** the server gets its own threat model in the invariant-ID format. Each enforced invariant cites the tests that pin it, and a test checks the document.

## Owner decisions

1. **The license of `server/`** (S4).
2. **Connector licenses:** whether independently written connectors may carry any license (C1).
3. **The board replacement policy** (S6), as proposed.
4. **Which plays go to external IRs** (C3). Proposed: plays played under an accepted preset only, never a what-if.
5. **Workers Paid and the allowances** that bound the monthly cost (S1, S5, S8).
6. **The first sign-in provider** (S7).
7. **Retention and privacy:** how long records are kept, and what a published replay shows (design.md "Open Questions").
8. **Storage for evaluation:** the form of authors' permission for it (design.md "Open Questions"). It decides which charts can be eligible at all.
9. **The first external IR** (C5): approaching an operator to accept tapestry as a client. The candidates are those whose route admits a new, truthfully named client: Tachi, BMZ IR, BMS-IR and rianIR.

## Work that can proceed before stage 4

This touches no server and no stage-2 file.

- **WASM build in CI:** a Linux CI job building `eval`, `bms-parser` and `replay` for `wasm32-unknown-unknown`. design.md already requires it ("Server Architecture"). setup.md defers the target to "the stage that uses it", and design.md's "kept passing in CI at all times" is the stronger statement.
