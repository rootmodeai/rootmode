# Using rootmode from your existing tools

Claude Code, Cursor, VS Code, Zed and Aider all let you point them at a
different server. rootmode can be that server: turn on **Use it elsewhere** in
the app and it opens a small HTTP endpoint on loopback that speaks the two
APIs those tools already know, translating each request into an ordinary
rootmode job.

Nothing about the network changes. The same routing runs — cheapest provider
serving that model, latency breaking ties — and results are still verified
against their sha256 before anything reaches your editor.

```
  editor  ──HTTP──▶  rootmode desktop  ──libp2p──▶  provider  ──▶  vLLM
          127.0.0.1        (routing, hash check)
```

## Turning it on

**Use it elsewhere** in the sidebar → **Turn on**. The screen then shows the
address, a key, and the exact lines to paste for each tool.

Two properties, both deliberate:

- **Loopback only.** The listener binds `127.0.0.1`. It is a door for programs
  on your machine, not a service you are accidentally hosting.
- **The key is always required.** Anything running as your user could
  otherwise find the port and spend your providers' time. Replacing the key
  from the same screen cuts off anything still holding the old one.

Default port is `11435` — one above Ollama's `11434`, so the two can be
installed side by side.

## Endpoints

| Path | Shape | Used by |
|---|---|---|
| `POST /v1/messages` | Anthropic Messages | Claude Code |
| `POST /v1/chat/completions` | OpenAI chat completions | Cursor, Continue, Cline, Zed, Aider, most others |
| `GET /v1/models` | both catalogue formats at once | model pickers |

The key travels as `x-api-key` (what Anthropic clients send) or
`Authorization: Bearer` (what OpenAI clients send); either is accepted on
every route.

Check it from a terminal:

```sh
curl http://127.0.0.1:11435/v1/models -H "Authorization: Bearer $KEY"
```

## Setup per tool

**Claude Code**

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:11435
export ANTHROPIC_AUTH_TOKEN=<key>
export ANTHROPIC_MODEL=<model rootmode serves>
claude
```

Claude Code's banner keeps showing a Claude model name — that label is its
own and does not follow the base URL. The request counter on the Connect
screen is what tells you the work is really going to the network.

**Cursor** — Settings → Models → override the OpenAI base URL with
`http://127.0.0.1:11435/v1`, paste the key, and add the model name by hand.
Cursor verifies the key by calling the endpoint, so leave rootmode running.

**VS Code (Continue)** — in `~/.continue/config.json`:

```json
{
  "models": [{
    "title": "rootmode",
    "provider": "openai",
    "model": "<model rootmode serves>",
    "apiBase": "http://127.0.0.1:11435/v1",
    "apiKey": "<key>"
  }]
}
```

Cline and Roo take the same three values in their settings panel.

**Zed** — in `settings.json`, an `openai` entry under `language_models` with
`api_url` set to `http://127.0.0.1:11435/v1`.

**Aider**

```sh
export OPENAI_API_BASE=http://127.0.0.1:11435/v1
export OPENAI_API_KEY=<key>
aider --model openai/<model rootmode serves>
```

Model names come from what the network is serving. The Connect screen fills
them in for you; `GET /v1/models` and the Chat tab's picker show the same
list.

## What it does not do

Worth knowing before you wire an agent to it:

- **Streaming is framed, not incremental.** A worker returns the whole answer,
  so the SSE event sequence is correct and complete but the text arrives in
  one delta. Clients parse it fine; you will not see it typed out.
- **Text only.** Image generation is a different job kind with no equivalent
  in either API, so it stays in the Images tab.
- **No thinking blocks, prompt caching, or server-side tools.** Those are
  Anthropic-side features and are not advertised.
- **Reasoning models spend your budget silently.** A model that thinks before
  answering bills that thinking against `max_tokens`, and the thinking is not
  returned. If answers arrive truncated, raise `max_tokens` — the error text
  says how many characters of reasoning were produced when nothing else was.
  A client that names no ceiling gets 16,384, chosen to leave room to think
  first; Anthropic clients always send their own, OpenAI ones often do not.
- **Token counts are billed with the OpenAI tokenizer.** Input, output,
  cached-input and reasoning tokens are counted locally, then raised to
  whatever the worker reported when that is higher — so an untrusted or
  OpenRouter worker cannot shrink the bill below what we can measure. Cache
  hits are the one figure only a provider can see, and are taken from it as a
  subset of input, never invented.

## Tool calls

Tool calls travel end to end, in both dialects. A client's tool definitions
are forwarded to the inference server, and the model's calls come back as
Anthropic `tool_use` blocks (`stop_reason: "tool_use"`) or OpenAI `tool_calls`
(`finish_reason: "tool_calls"`), streamed or not.

This is what makes agentic editors work at all rather than work well. Without
it the model answers by calling a tool, nothing is listening, and the job
comes back empty — which is a hard failure, not a degradation.

Two things the bridge cannot do:

- **Anthropic server-side tools are dropped.** `web_search`, `computer` and
  friends have no `input_schema` and no rootmode worker can run them.
  Client-side tools — the ones an editor actually implements — pass through.
- **The worker must support it.** Tool schemas ride in a field added after v1
  shipped. An older worker ignores it and answers as it always did, which
  looks exactly like the empty-completion failure above. Rebuild the worker.

How well a given model *uses* the tools is the model's business, not the
bridge's. Plumbing being correct is a precondition for a usable agent, not a
guarantee of one.

## Mid-conversation system messages

Anthropic lets an operator put a `system` message in the middle of a
conversation, and Claude Code leans on it — a reminder after a tool result, a
mode switch mid-session. Most chat templates on the other side accept a system
message in first position only, and some answer a misplaced one with an empty
generation, which surfaces as "the server returned an empty completion" and is
impossible to diagnose from the client.

So anything after the opening system message is folded into the nearest user
turn, wrapped in `<system-reminder>` tags, keeping its position. The text is
never dropped — it is an instruction the user meant.

## Unknown model names

Editors do not only send the model you configured. Claude Code names its own
small model for background work such as titling a conversation, and no
rootmode provider will ever serve anything by that name. Refusing those
requests makes the editor look broken for a reason you cannot act on, so by
default an unrecognised name is answered by the cheapest model on offer, and
the reply says which model actually answered.

Turn it off under **Details** on the Connect screen if you would rather have
anything unrecognised refused with a `404` listing what is available.

## Errors

Failures come back in whichever shape the caller used, with the status code
that shape expects: `401` for a missing or wrong key, `400` for a request that
will not map, `404` when nobody serves the model asked for and substitution is off — that
one lists what *is* on offer, so a wrong model name is self-correcting — and `502` when
a provider fails or hangs up mid-job.

If the endpoint will not start, the usual cause is the port already being in
use; the app says so on the same screen and you can change it under Details.

## Tracing a failing client

When a tool fails only in its own hands, guessing at what it sent is how an
afternoon disappears. Start the app with `ROOTMODE_GATEWAY_TRACE` set to a
file path and every *failed* request is appended to it verbatim, with the
error:

```sh
ROOTMODE_GATEWAY_TRACE=/tmp/rootmode-trace.log npm run app
```

Off unless set, because prompts are the most sensitive thing this app
handles and nobody should discover later that they were on disk. Successful
requests are never written.
