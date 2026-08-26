import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, errorText, events } from "../lib/api";
import { useStore } from "../lib/store";
import { useEvent } from "../lib/useEvent";
import type {
  Attachment,
  ChatMessage,
  Conversation,
  LlmParams,
  Message,
  PotStatus,
  ProviderOption,
} from "../lib/types";
import { Glider } from "../components/Glider";
import { DeleteAllChats } from "../components/DeleteAllChats";
import { MarkdownBody } from "../components/Markdown";
import { useChoice, usePick } from "../lib/choice";
import { describe, targetFor } from "../lib/models";
import {
  FundingHint,
  FundingNotice,
  fundingKindFromText,
  noticeFromCheck,
  usdExact,
} from "../components/FundingNotice";

/**
 * The main screen: ask a question, get an answer, and find the conversation
 * again tomorrow.
 *
 * Which machine ran it and which key signed it are recorded on every message,
 * but they are shown small and after the fact. You should be able to use this
 * without knowing what a peer is.
 */
/// How much room to give an answer.
///
/// Generous on purpose. A reasoning model spends tokens thinking before it
/// writes a word, and that thinking is billed against this number but never
/// shown — so a ceiling that looks ample for a reply can be consumed entirely
/// by reasoning, and the answer never arrives. A ceiling costs nothing when
/// it is not reached: the model stops when it is done.
const ANSWER_CEILING = 16_384;

/// Must match `rootmode_core::protocol::STOPPED` exactly — the one string a
/// worker and this client agree means "you asked for this," not "this broke".
const STOPPED_ERROR = "stopped by client";

/// Survives a remount, `useState` does not. Switching to another tab and back
/// unmounts this screen — see `App.tsx`, which renders it as
/// `{screen === "chat" && <Chat />}` rather than hiding it — so a fresh
/// `useState([])` briefly has nothing while the history reloads. A running
/// job has no such gap: it lives in the global store, so its streaming
/// bubble appears the instant you're back, ahead of the history it belongs
/// under. This is what closes that gap — the last known messages for a
/// conversation are available before the first paint, not after a round
/// trip, so there is nothing left for the fetch to visibly replace.
const messageCache = new Map<string, Message[]>();

export function Chat() {
  const { peers, jobs, settings, setSetting, streams, clearStream } = useStore();
  const [chats, setChats] = useState<Conversation[]>([]);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [pending, setPending] = useState<{ chatId: string; jobId: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  // What a Stop click leaves behind: the answer as far as it got, kept on
  // screen until something replaces it, since the worker was told to throw
  // the rest away rather than to finish quietly.
  const [stoppedNotice, setStoppedNotice] = useState<{ text: string; thinking: string } | null>(
    null,
  );
  const [pot, setPot] = useState<PotStatus | null>(null);
  const [funding, setFunding] = useState<{
    kind: "cap" | "empty" | "chain";
    reason: string;
    capMicros: number;
  } | null>(null);
  const endRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  /// Whether the view is following the answer. Reading back through what was
  /// already said is a deliberate act, so a token arriving must not yank the
  /// page down again — scrolling away turns following off, and scrolling back
  /// to the bottom turns it on.
  const following = useRef(true);

  // You choose a model; the backend chooses the provider — cheapest, then
  // fastest. Refreshed as providers come and go.
  const [providers, setProviders] = useState<ProviderOption[]>([]);
  /// null means "let rootmode choose", which is the default and the right one.
  const [chosen] = useChoice("llm");
  const picked = usePick("llm");
  const draftRef = useRef<HTMLTextAreaElement | null>(null);

  // Chosen by hand: remembered for next time, and the cursor goes where
  // the next thing happens.
  useEffect(() => {
    if (chosen) void setSetting("default_llm_model", chosen.model);
  }, [chosen, setSetting]);
  useEffect(() => {
    if (picked) draftRef.current?.focus();
  }, [picked]);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .availableProviders("llm")
        .then((rows) => !cancelled && setProviders(rows))
        .catch(() => undefined);
    void load();
    const timer = setInterval(load, 8000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [peers.length]);

  useEffect(() => {
    void api.potStatus().then(setPot).catch(() => undefined);
  }, []);

  // What was picked by hand, else the remembered default, else the cheapest
  // on offer — `providers` arrives sorted that way.
  const option = useMemo(() => {
    if (chosen) return chosen;
    const wanted = settings?.default_llm_model;
    return providers.find((p) => p.model === wanted) ?? providers[0];
  }, [providers, chosen, settings?.default_llm_model]);

  const model = option?.model;

  const loadChats = useCallback(async () => {
    const rows = await api.listConversations("llm");
    setChats(rows);
    return rows;
  }, []);

  useEffect(() => {
    loadChats()
      .then((rows) => setCurrentId((id) => id ?? rows[0]?.id ?? null))
      .catch((e) => setError(errorText(e)));
  }, [loadChats]);

  // Before paint, not after: a job already streaming is visible the instant
  // this screen exists (it lives in the global store), and the history has
  // to be too, or there is a frame — sometimes a long one, once a query has
  // to compete with everything a running job is also asking the database to
  // do — where a real answer looks like it lost everything above it.
  useLayoutEffect(() => {
    setStoppedNotice(null);
    setMessages(currentId ? (messageCache.get(currentId) ?? []) : []);
  }, [currentId]);

  useEffect(() => {
    if (!currentId) return;
    let cancelled = false;
    api
      .conversationMessages(currentId)
      .then((rows) => {
        if (cancelled) return;
        messageCache.set(currentId, rows);
        setMessages(rows);
      })
      .catch((e) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [currentId]);

  // Derived from the store, not held here: this screen unmounts whenever you
  // look at another tab, and a spinner that lives in it would vanish with it
  // while the work carried on.
  const running = useMemo(
    () =>
      currentId
        ? jobs.find(
            (j) =>
              j.conversation_id === currentId &&
              (j.status === "queued" || j.status === "running"),
          )
        : undefined,
    [jobs, currentId],
  );
  // Prefer whatever the store says is running for this chat; fall back to the
  // id we just submitted, for the moment before the first event lands.
  const pendingHere = pending && pending.chatId === currentId ? pending.jobId : null;
  const job =
    running ?? (pendingHere ? jobs.find((j) => j.job_id === pendingHere) : undefined);
  const waiting = !!running || !!pendingHere;

  useEffect(() => {
    if (!pending) return;
    const j = jobs.find((x) => x.job_id === pending.jobId);
    if (j && (j.status === "done" || j.status === "failed")) {
      setPending((p) => (p?.jobId === j.job_id ? null : p));
    }
  }, [jobs, pending]);

  /// The buffer belongs to the job, not to this component, so coming back to
  /// a chat mid-answer shows the whole of it.
  const liveId = running?.job_id ?? pendingHere;
  const live = liveId ? (streams[liveId] ?? null) : null;

  useEffect(() => {
    if (!following.current) return;
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages.length, waiting, live?.text, live?.thinking]);

  // Reasoning can arrive on its own channel, inline in the text, or both.
  const liveSplit = useMemo(() => {
    const split = splitThinking(live?.text ?? "");
    return {
      thinking: [live?.thinking ?? "", split.thinking].filter(Boolean).join("\n"),
      text: split.text,
    };
  }, [live?.text, live?.thinking]);

  const stoppedSplit = useMemo(
    () => splitThinking(stoppedNotice?.text ?? ""),
    [stoppedNotice?.text],
  );
  const stoppedThought = [stoppedNotice?.thinking ?? "", stoppedSplit.thinking]
    .filter(Boolean)
    .join("\n");

  // A little slack, because "at the bottom" is never exact once smooth
  // scrolling and sub-pixel heights are involved.
  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    following.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }

  // Documents are read by the backend when the OS reports a drop; all this
  // does is show what came back.
  useEvent(events.onFilesDropped, (outcome) => {
    if (outcome.attached.length > 0) {
      setAttachments((prev) => {
        const names = new Set(prev.map((a) => a.name));
        return [...prev, ...outcome.attached.filter((a) => !names.has(a.name))];
      });
    }
    setError(outcome.rejected.length > 0 ? outcome.rejected.join(" · ") : null);
  });

  // The reply is written by the job pipeline, so it lands whether or not this
  // screen is open. All that is left here is showing it.
  useEvent(events.onMessage, (message) => {
    setPending((p) => (p && p.jobId === message.job_id ? null : p));
    if (message.job_id) clearStream(message.job_id);
    if (message.conversation_id !== currentId) {
      const cached = messageCache.get(message.conversation_id);
      if (cached && !cached.some((m) => m.id === message.id)) {
        messageCache.set(message.conversation_id, [...cached, message]);
      }
      return;
    }
    setMessages((prev) => {
      const next = prev.some((m) => m.id === message.id) ? prev : [...prev, message];
      messageCache.set(message.conversation_id, next);
      return next;
    });
    setStoppedNotice(null);
    void loadChats();
  });

  // A failure has no reply to file, so it is reported here — except a stop,
  // which is not a failure the user needs told about, only an answer that
  // ends where they asked it to.
  useEffect(() => {
    if (job?.status !== "failed") return;
    if (job.error === STOPPED_ERROR) {
      setStoppedNotice({ text: live?.text ?? "", thinking: live?.thinking ?? "" });
    } else {
      const text = job.error ?? "That didn't work.";
      const kind = fundingKindFromText(text);
      if (kind && kind !== "ok") {
        setFunding({
          kind,
          reason: text,
          capMicros: pot?.max_per_job_micros ?? 0,
        });
        setError(null);
      } else {
        setError(text);
      }
    }
    setPending((p) => (p && p.jobId === job.job_id ? null : p));
    clearStream(job.job_id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [job]);

  async function send() {
    const text = draft.trim();
    if (waiting) return;
    if (!text && attachments.length === 0) return;
    if (!option) {
      setError("No providers are online right now.");
      return;
    }
    try {
      const check = await api.potCheck(option.price, option.unpriced, "llm");
      const blocked = noticeFromCheck(check);
      if (blocked) {
        setFunding(blocked);
        setError(null);
        return;
      }
    } catch (e) {
      setError(errorText(e));
      return;
    }
    setError(null);
    setFunding(null);
    setDraft("");
    setStoppedNotice(null);
    following.current = true;

    try {
      // First message names the chat.
      let chatId = currentId;
      if (!chatId) {
        const created = await api.newConversation(text, "llm");
        chatId = created.id;
        setCurrentId(chatId);
        setChats((prev) => [created, ...prev]);
      }

      // Documents go into the message itself, not alongside it: the model
      // sees the history on every turn, so an attachment kept out of the
      // content would vanish after the first question about it.
      const content =
        attachments.length > 0
          ? `${attachments.map(documentBlock).join("\n\n")}\n\n${text}`
          : text;

      const saved = await api.addMessage({
        conversationId: chatId,
        role: "user",
        content,
      });
      setAttachments([]);
      const history: ChatMessage[] = [...messages, saved].map((m) => ({
        role: m.role,
        content: m.content,
      }));
      setMessages((prev) => {
        const next = [...prev, saved];
        messageCache.set(chatId, next);
        return next;
      });

      const payload: LlmParams = {
        kind: "llm",
        model_id: model,
        messages: history,
        max_tokens: ANSWER_CEILING,
        temperature: 0.7,
      };
      const record = await api.submitJob(targetFor(option, providers).peer_id, payload, chatId);
      setPending({ chatId, jobId: record.job_id });
    } catch (e) {
      const text = errorText(e);
      const kind = fundingKindFromText(text);
      if (kind && kind !== "ok") {
        setFunding({
          kind,
          reason: text,
          capMicros: pot?.max_per_job_micros ?? 0,
        });
      } else {
        setError(text);
      }
    }
  }

  function stop() {
    if (job) void api.stopJob(job.job_id).catch(() => undefined);
  }

  async function removeChat(id: string) {
    await api.deleteConversation(id);
    messageCache.delete(id);
    const rows = await loadChats();
    if (currentId === id) setCurrentId(rows[0]?.id ?? null);
  }

  return (
    <div className="chat">
      <aside className="chat-list">
        <div className="chat-list-head">
          <button
            className="btn"
            style={{ width: "100%" }}
            onClick={() => {
              setCurrentId(null);
              setMessages([]);
              setError(null);
              setFunding(null);
            }}
          >
            + New chat
          </button>
        </div>
        <div className="chat-list-items">
          {chats.length === 0 ? (
            <div className="empty" style={{ padding: "24px 8px", fontSize: 13 }}>
              No chats yet
            </div>
          ) : (
            chats.map((c) =>
              confirming === c.id ? (
                <div key={c.id} className="chat-row confirming">
                  <div className="t">Delete this chat?</div>
                  <div className="row" style={{ gap: 6, marginTop: 6 }}>
                    <button
                      className="btn sm danger"
                      onClick={() => {
                        setConfirming(null);
                        void removeChat(c.id);
                      }}
                    >
                      Delete
                    </button>
                    <button className="btn sm ghost" onClick={() => setConfirming(null)}>
                      Cancel
                    </button>
                  </div>
                </div>
              ) : (
                <div key={c.id} className={`chat-row ${c.id === currentId ? "active" : ""}`}>
                  <button className="chat-item" onClick={() => setCurrentId(c.id)}>
                    <div className="t">{c.title}</div>
                    <div className="p">{c.preview || "…"}</div>
                  </button>
                  <button
                    className="chat-del"
                    title="Delete chat"
                    aria-label={`Delete ${c.title}`}
                    onClick={() => setConfirming(c.id)}
                  >
                    ×
                  </button>
                </div>
              ),
            )
          )}
        </div>
        <DeleteAllChats
          onDone={() => {
            setCurrentId(null);
            setMessages([]);
            setError(null);
            setFunding(null);
            void loadChats();
          }}
        />
      </aside>

      <section className="thread">
        <div className="thread-scroll" ref={scrollRef} onScroll={onScroll}>
          <div className="thread-inner">
            {funding && (
              <FundingNotice
                kind={funding.kind}
                reason={funding.reason}
                capMicros={funding.capMicros}
                onActionError={setError}
              />
            )}
            {error && !funding && (
              <div className="note bad" style={{ marginBottom: 16 }}>
                {error}
              </div>
            )}

            {messages.length === 0 && !waiting ? (
              <Welcome provider={option?.peer_label} model={model ? describe(model).name : model} />
            ) : (
              messages.map((m) => {
                const split =
                  m.role === "assistant"
                    ? splitThinking(m.content)
                    : { thinking: "", text: m.content };
                const thought = m.thinking || split.thinking;
                return (
                <div key={m.id} className={`msg ${m.role}`}>
                  <div className="who">{m.role === "user" ? "You" : "Answer"}</div>
                  {thought && <Thinking text={thought} live={false} />}
                  <MessageBody content={split.text} />
                  <div className="msg-foot">
                    {m.role === "assistant" && (
                      <div className="meta">
                        {m.model ? <span title={m.model}>{describe(m.model).name}</span> : "unknown model"}
                        {m.peer ? ` · ${m.peer}` : ""}
                        {m.tokens ? ` · ${m.tokens.toLocaleString()} tokens` : ""}
                        {/* The bill for this exact reply. Absent on free
                            providers — money that never moved is not shown. */}
                        {m.cost_micros != null ? ` · ${usdExact(m.cost_micros)}` : ""}
                        {m.sha256 ? ` · ${m.sha256.slice(0, 10)}` : ""}
                      </div>
                    )}
                    <CopyText text={split.text} />
                  </div>
                </div>
                );
              })
            )}

            {job && job.status !== "done" && (
              <div className="msg assistant">
                <div className="who">Answer</div>
                {liveSplit.thinking ? <Thinking text={liveSplit.thinking} live /> : null}
                {liveSplit.text ? <MarkdownBody text={liveSplit.text} /> : null}
                {/* The thinking box says "Thinking…" itself, with the
                    glider running beside it; a second line underneath saying
                    the same word is the app repeating itself. */}
                {!liveSplit.text && !liveSplit.thinking && (
                  <div className="meta">
                    {job.status === "queued" ? "Waiting for a free slot…" : "Starting…"}
                  </div>
                )}
              </div>
            )}

            {stoppedNotice && (
              <div className="msg assistant">
                <div className="who">Answer</div>
                {stoppedThought && <Thinking text={stoppedThought} live={false} />}
                {stoppedSplit.text && <MarkdownBody text={stoppedSplit.text} />}
                <div className="msg-foot">
                  <span className="stopped-tag">Stopped</span>
                  {stoppedSplit.text && <CopyText text={stoppedSplit.text} />}
                </div>
              </div>
            )}
            <div ref={endRef} />
          </div>
        </div>

        <div className="composer">
          <div className="composer-inner">
            {attachments.length > 0 && (
              <div className="attachments">
                {attachments.map((a) => (
                  <span key={a.name} className="attachment" title={`${a.chars.toLocaleString()} characters read`}>
                    <span className="doc-icon">▤</span>
                    {a.name}
                    {a.truncated && <span className="attachment-cut">shortened</span>}
                    <button
                      aria-label={`Remove ${a.name}`}
                      onClick={() => setAttachments((prev) => prev.filter((x) => x.name !== a.name))}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>
            )}
            <div className="composer-box">
              <textarea
                ref={draftRef}
                rows={1}
                value={draft}
                placeholder={
                  option
                    ? attachments.length > 0
                      ? "Ask about the document…"
                      : "Ask anything, or drop a document in…"
                    : "No providers online"
                }
                disabled={!option}
                onChange={(e) => {
                  setDraft(e.target.value);
                  e.target.style.height = "auto";
                  e.target.style.height = `${Math.min(e.target.scrollHeight, 200)}px`;
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void send();
                  }
                }}
              />
              {waiting ? (
                <button className="stop" onClick={stop} title="Stop generating">
                  <span className="stop-icon" />
                </button>
              ) : (
                <button
                  className="send"
                  onClick={() => void send()}
                  disabled={!draft.trim() || !option}
                  title="Send"
                >
                  ↑
                </button>
              )}
            </div>
            <div className="composer-hint">
              {option && !option.unpriced && option.price > 0 && pot?.client && (
                <FundingHint
                  capMicros={
                    pot.max_per_job_micros > 0 ? pot.max_per_job_micros : 500_000
                  }
                />
              )}
            </div>
          </div>
        </div>
      </section>
    </div>
  );
}

/** Per million tokens, in whatever the provider quoted. */

/// How a document is carried in a message. Mirrors the backend's wrapper, so
/// the model sees the document named and fenced off from the question.
function documentBlock(a: Attachment): string {
  const note = a.truncated
    ? `\n[cut off at ${a.chars} characters — this is the beginning of the document only]`
    : "";
  return `<document name="${a.name}">\n${a.text}${note}\n</document>`;
}

/// Split a stored message back into its documents and the question, so a long
/// attachment does not bury the sentence the person actually typed.
function splitDocuments(content: string): { docs: Array<{ name: string; body: string }>; text: string } {
  const docs: Array<{ name: string; body: string }> = [];
  const text = content
    .replace(/<document name="([^"]*)">\n([\s\S]*?)\n<\/document>/g, (_m, name, body) => {
      docs.push({ name, body });
      return "";
    })
    .trim();
  return { docs, text };
}

/// Copy a message's text. Appears on hover so it does not clutter a
/// conversation you are only reading.
/// The model's private notes. Closed until you ask — most of the time you
/// only want the answer. Once open, new tokens keep filling it.
/**
 * Some models mark their reasoning inline instead of on a separate channel —
 * Qwen writes `<think>…</think>` straight into the text. Left alone it reads
 * as the answer, which it isn't, so it is lifted out here and shown in the
 * same box as reasoning that arrived properly separated. The tag can still be
 * open while tokens stream: everything after it is thinking, so far.
 */
const THINK_OPEN = /<(think|thinking|reasoning)>/i;
const THINK_CLOSE = /<\/(think|thinking|reasoning)>/i;

export function splitThinking(raw: string): { thinking: string; text: string } {
  let thinking = "";
  let text = raw;

  // The common Qwen case first: the chat template ends the prompt with an
  // open `<think>`, so the model never emits one — the reasoning simply is
  // the start of the reply, and the only marker is the `</think>` that ends
  // it. A closing tag with nothing opening it means everything before it was
  // thinking.
  const orphan = THINK_CLOSE.exec(text);
  if (orphan && (!THINK_OPEN.exec(text) || THINK_OPEN.exec(text)!.index > orphan.index)) {
    thinking += text.slice(0, orphan.index);
    text = text.slice(orphan.index + orphan[0].length);
  }

  for (;;) {
    const open = THINK_OPEN.exec(text);
    if (!open) break;
    const rest = text.slice(open.index);
    const close = new RegExp(`</${open[1]}>`, "i").exec(rest);
    if (!close) {
      // Still being written — the tail is thinking until the tag closes.
      thinking += rest.slice(open[0].length);
      text = text.slice(0, open.index);
      break;
    }
    thinking += rest.slice(open[0].length, close.index);
    text = text.slice(0, open.index) + text.slice(open.index + close.index + close[0].length);
  }
  return { thinking: thinking.trim(), text: text.trimStart() };
}

function Thinking({ text, live }: { text: string; live: boolean }) {
  return (
    <details className="thinking">
      <summary>
        {live ? "Thinking…" : "Thought"}
        {live && <Glider size={13} className="thinking-icon" animate />}
      </summary>
      <div className="thinking-body">{text}</div>
    </details>
  );
}

function CopyText({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="copy-text"
      title="Copy this message"
      aria-label={copied ? "Copied" : "Copy this message"}
      onClick={() => {
        void navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1400);
      }}
    >
      {copied ? "copied" : "copy"}
    </button>
  );
}

function MessageBody({ content }: { content: string }) {
  const { docs, text } = splitDocuments(content);
  return (
    <>
      {docs.map((d) => (
        <details key={d.name} className="doc">
          <summary>
            <span className="doc-icon">▤</span> {d.name}
            <span className="doc-size">{d.body.length.toLocaleString()} characters</span>
          </summary>
          <pre>{d.body}</pre>
        </details>
      ))}
      {text && <MarkdownBody text={text} />}
    </>
  );
}

function Welcome({ provider, model }: { provider?: string; model?: string }) {
  return (
    <div style={{ textAlign: "center", padding: "60px 20px" }}>
      <div className="boot-mark" style={{ marginBottom: 18 }}>
        <Glider size={42} />
      </div>
      <h2 style={{ fontSize: 19, margin: "0 0 6px", letterSpacing: "-0.01em" }}>
        What would you like to know?
      </h2>
      <p style={{ color: "var(--text-2)", margin: 0 }}>
        {provider
          ? `Running on ${provider}${model ? ` · ${model}` : ""}`
          : "Nobody is online to answer yet."}
      </p>
    </div>
  );
}
