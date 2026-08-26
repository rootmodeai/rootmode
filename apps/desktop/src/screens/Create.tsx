import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { api, errorText, events } from "../lib/api";
import { useStore } from "../lib/store";
import { useEvent } from "../lib/useEvent";
import type { Conversation, ImageParams, JobPayload, Message, ProviderOption } from "../lib/types";
import { Glider } from "../components/Glider";
import { DeleteAllChats } from "../components/DeleteAllChats";
import { ProviderPicker } from "../components/ProviderPicker";
import { describe, targetFor } from "../lib/models";
import {
  FundingHint,
  FundingNotice,
  fundingKindFromText,
  noticeFromCheck,
} from "../components/FundingNotice";
import type { PotStatus } from "../lib/types";

const STOPPED_ERROR = "stopped by client";

/**
 * Making things, laid out exactly like Chat.
 *
 * The same shape on purpose: a list of sessions on the left, a thread in the
 * middle, a box at the bottom. Somebody who has used one screen has used the
 * other, and a picture you asked for belongs in the conversation where you
 * asked for it rather than in a separate gallery you have to go and find.
 */
export function Create({ kind }: { kind: "image" | "video" }) {
  const noun = kind === "video" ? "video" : "picture";
  const plural = kind === "video" ? "videos" : "pictures";
  const { jobs } = useStore();
  const [chats, setChats] = useState<Conversation[]>([]);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [pending, setPending] = useState<{ chatId: string; jobId: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [pot, setPot] = useState<PotStatus | null>(null);
  const [funding, setFunding] = useState<{
    kind: "cap" | "empty" | "chain";
    reason: string;
    capMicros: number;
  } | null>(null);

  const endRef = useRef<HTMLDivElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const draftRef = useRef<HTMLTextAreaElement | null>(null);
  /// Same rule as Chat: following the bottom is the default, but scrolling
  /// back to look at an earlier picture must not be undone by the next one.
  const following = useRef(true);

  const [offers, setOffers] = useState<ProviderOption[]>([]);
  /// null means "let rootmode choose" — cheapest, then fastest.
  const [chosen, setChosen] = useState<ProviderOption | null>(null);

  useEffect(() => {
    setChosen(null);
    const load = () =>
      void api
        .availableProviders(kind)
        .then(setOffers)
        .catch(() => undefined);
    load();
    const t = setInterval(load, 8000);
    return () => clearInterval(t);
  }, [kind]);

  useEffect(() => {
    void api.potStatus().then(setPot).catch(() => undefined);
  }, []);

  // Picked by hand, else the cheapest on offer — `offers` arrives sorted.
  const option = chosen ?? offers[0];
  const provider = option;
  const checkpoint = option?.model;

  const loadChats = useCallback(async () => {
    const rows = await api.listConversations(kind);
    setChats(rows);
    return rows;
  }, [kind]);

  useEffect(() => {
    void loadChats().then((rows) => setCurrentId((id) => id ?? rows[0]?.id ?? null));
  }, [loadChats]);

  useEffect(() => {
    setError(null);
    setFunding(null);
  }, [currentId]);

  useEffect(() => {
    let cancelled = false;
    if (!currentId) {
      setMessages([]);
      return;
    }
    void api
      .conversationMessages(currentId)
      .then((rows) => {
        if (!cancelled) setMessages(rows);
      })
      .catch((e) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [currentId]);

  // Same as Chat: the picture is filed by the job pipeline, so it arrives
  // whether or not this screen is the one on show.
  useEvent(events.onMessage, (message) => {
    setPending((p) => (p && p.jobId === message.job_id ? null : p));
    if (message.conversation_id !== currentId) return;
    setMessages((prev) => (prev.some((m) => m.id === message.id) ? prev : [...prev, message]));
    void loadChats();
  });

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

  /// The chain is the conversation. Whatever this session drew last is what
  /// the next prompt works on — no button to press, because "carry on from
  /// what we were just looking at" is what somebody means by typing again.
  /// Starting over is what "+ New set" is for.
  const lastPicture = useMemo(
    () =>
      kind === "image"
        ? [...messages].reverse().find((m) => m.role === "assistant" && m.job_id)?.job_id ?? null
        : null,
    [messages, kind],
  );

  useEffect(() => {
    if (job?.status !== "failed") return;
    if (job.error !== STOPPED_ERROR) {
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
    setPending((p) => (p && job.job_id === p.jobId ? null : p));
  }, [job]);

  useEffect(() => {
    if (!following.current) return;
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages.length, waiting]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    following.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }

  function stop() {
    const id = job?.job_id ?? pendingHere;
    if (id) void api.stopJob(id).catch(() => undefined);
  }

  function reusePrompt(text: string) {
    setDraft(text);
    requestAnimationFrame(() => {
      const el = draftRef.current;
      if (!el) return;
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 180)}px`;
      el.focus();
      el.setSelectionRange(text.length, text.length);
    });
  }

  async function generate() {
    const text = draft.trim();
    if (!text || waiting) return;
    if (!provider) {
      setError(`Nobody is offering to make ${plural} right now.`);
      return;
    }
    try {
      const check = await api.potCheck(provider.price, provider.unpriced, kind);
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
    following.current = true;

    try {
      let chatId = currentId;
      if (!chatId) {
        const created = await api.newConversation(text, kind);
        chatId = created.id;
        setCurrentId(chatId);
        setChats((prev) => [created, ...prev]);
      }

      const saved = await api.addMessage({
        conversationId: chatId,
        role: "user",
        content: text,
      });
      setMessages((prev) => [...prev, saved]);

      const payload: JobPayload =
        kind === "video"
          ? {
              kind: "video",
              checkpoint_id: checkpoint,
              prompt: text,
            }
          : await (async (): Promise<ImageParams> => {
              const image: ImageParams = {
                kind: "image",
                checkpoint_id: checkpoint,
                prompt: text,
              };
              if (lastPicture) {
                // The file can be gone if it was deleted; a fresh picture is
                // a better answer than refusing to draw.
                image.from_image = await api
                  .readResultBytes(lastPicture)
                  .catch(() => undefined);
              }
              return image;
            })();
      const record = await api.submitJob(targetFor(option, offers).peer_id, payload, chatId);
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

  async function removeChat(id: string) {
    await api.deleteConversation(id);
    const rows = await loadChats();
    if (currentId === id) setCurrentId(rows[0]?.id ?? null);
  }

  async function removeImage(message: Message) {
    if (!message.job_id) return;
    try {
      await api.deleteResult(message.job_id);
      setMessages((prev) => prev.filter((m) => m.id !== message.id));
      void loadChats();
    } catch (e) {
      setError(errorText(e));
    }
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
              setChosen(null);
            }}
          >
            {kind === "video" ? "+ New video" : "+ New image"}
          </button>
        </div>
        <div className="chat-list-items">
          {chats.length === 0 ? (
            <div className="empty" style={{ padding: "24px 8px", fontSize: 13 }}>
              {kind === "video" ? "No videos yet" : "No pictures yet"}
            </div>
          ) : (
            chats.map((c) =>
              confirming === c.id ? (
                <div key={c.id} className="chat-row confirming">
                  <div className="t">Delete this set?</div>
                  <div style={{ fontSize: 11.5, color: "var(--text-3)", marginTop: 4 }}>
                    The files are erased, not just forgotten.
                  </div>
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
                  <button
                    className="chat-item"
                    onClick={() => setCurrentId(c.id)}
                  >
                    <div className="t">{c.title}</div>
                    <div className="p">{c.preview || "…"}</div>
                  </button>
                  <button
                    className="chat-del"
                    title={kind === "video" ? "Delete this video set" : "Delete these pictures"}
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
            setChosen(null);
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
              <Welcome noun={noun} />
            ) : (
              messages.map((m) =>
                m.role === "user" ? (
                  <div key={m.id} className="msg user">
                    <div className="who">You</div>
                    <div className="bubble">{m.content}</div>
                    <div className="msg-foot">
                      <CopyText text={m.content} />
                      <button
                        className="copy-text always"
                        type="button"
                        onClick={() => reusePrompt(m.content)}
                      >
                        use again
                      </button>
                    </div>
                  </div>
                ) : (
                  <Picture
                    key={m.id}
                    kind={kind}
                    message={m}
                    onDelete={() => void removeImage(m)}
                  />
                ),
              )
            )}

            {job && job.status !== "done" && job.status !== "failed" && (
              <MediaCup
                kind={kind}
                progress={job.progress}
                status={job.status}
                onStop={stop}
              />
            )}

            <div ref={endRef} />
          </div>
        </div>

        <div className="composer">
          <div className="composer-inner">
            <div className="composer-box">
              <textarea
                ref={draftRef}
                rows={1}
                value={draft}
                placeholder={
                  !provider
                    ? `Nobody is making ${noun === "video" ? "videos" : "pictures"} right now`
                    : lastPicture
                      ? "What should change?"
                      : `Describe a ${noun}…`
                }
                onChange={(e) => {
                  setDraft(e.target.value);
                  e.target.style.height = "auto";
                  e.target.style.height = `${Math.min(e.target.scrollHeight, 180)}px`;
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void generate();
                  }
                }}
              />
              {waiting ? (
                <button className="stop" onClick={stop} title="Stop this job">
                  <span className="stop-icon" />
                </button>
              ) : (
                <button
                  className="btn primary"
                  disabled={!draft.trim() || !provider}
                  onClick={() => void generate()}
                >
                  {lastPicture
                    ? "Change it"
                    : kind === "video"
                      ? "Make"
                      : "Draw"}
                </button>
              )}
            </div>
            <div className="composer-hint">
              <ProviderPicker kind={kind} value={chosen} onChange={setChosen} />
              {provider && !provider.unpriced && provider.price > 0 && pot?.client && (
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

/** One generated picture or clip, with the way to get rid of it. */
function Picture({
  kind,
  message,
  onDelete,
}: {
  kind: "image" | "video";
  message: Message;
  onDelete: () => void;
}) {
  const [media, setMedia] = useState<string | null>(null);
  const [gone, setGone] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [zoom, setZoom] = useState<{ x: number; y: number } | null>(null);
  const noun = kind === "video" ? "video" : "picture";

  useEffect(() => {
    let cancelled = false;
    if (!message.job_id) return;
    void api
      .readResultImage(message.job_id)
      .then((dataUrl) => {
        if (cancelled) return;
        setMedia(dataUrl);
        setZoom(null);
      })
      // The row can outlive the file if a delete half-failed; say so rather
      // than showing a broken frame.
      .catch(() => !cancelled && setGone(true));
    return () => {
      cancelled = true;
    };
  }, [message.job_id]);

  return (
    <div className="msg assistant">
      <div className="who">{kind === "video" ? "Video" : "Picture"}</div>

      {gone ? (
        <div className="empty" style={{ fontSize: 13 }}>
          This {noun} is no longer on disk.
        </div>
      ) : (
        <div
          className={`preview${zoom ? " zoomed" : ""}`}
          style={
            zoom
              ? ({ "--ox": `${zoom.x}%`, "--oy": `${zoom.y}%` } as CSSProperties)
              : undefined
          }
        >
          {media ? (
            looksLikeVideo(media) ? (
              <video src={media} controls autoPlay loop muted playsInline />
            ) : (
              <img
                src={media}
                alt=""
                draggable={false}
                title={zoom ? "Click to zoom out" : "Click to zoom in"}
                onClick={(e) => {
                  if (zoom) {
                    setZoom(null);
                    return;
                  }
                  const r = e.currentTarget.getBoundingClientRect();
                  setZoom({
                    x: ((e.clientX - r.left) / r.width) * 100,
                    y: ((e.clientY - r.top) / r.height) * 100,
                  });
                }}
              />
            )
          ) : (
            <div className="empty">Loading…</div>
          )}
        </div>
      )}

      {/* Under the picture, never over it: a control sitting on the image
          covers the thing you are trying to look at. */}
      {!gone && (
        <div className="picture-actions">
          {message.job_id && (
            <button
              className="btn sm ghost"
              onClick={() => void api.revealResult(message.job_id!).catch(() => undefined)}
            >
              Show in Finder
            </button>
          )}
          {confirming ? (
            <>
              <button
                className="btn sm danger"
                onClick={() => {
                  setConfirming(false);
                  onDelete();
                }}
              >
                Erase it
              </button>
              <button className="btn sm ghost" onClick={() => setConfirming(false)}>
                Keep
              </button>
            </>
          ) : (
            <button className="btn sm ghost" onClick={() => setConfirming(true)}>
              Delete
            </button>
          )}
        </div>
      )}

      <details className="advanced">
        <summary>Details</summary>
        <div className="body mono" style={{ fontSize: 12, color: "var(--text-2)" }}>
          <div>sha256 {message.sha256 ?? "—"}</div>
          {message.model && <div title={message.model}>{describe(message.model).name}</div>}
          {message.peer && <div>made by {message.peer}</div>}
        </div>
      </details>
    </div>
  );
}

function CopyText({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="copy-text always"
      type="button"
      title="Copy this prompt"
      aria-label={copied ? "Copied" : "Copy this prompt"}
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

function looksLikeVideo(src: string) {
  return src.startsWith("data:video/") || /\.(mp4|webm|mov)(\?|#|$)/i.test(src);
}

/** Fills from the bottom while the worker is still making it. */
function MediaCup({
  kind,
  progress,
  status,
  onStop,
}: {
  kind: "image" | "video";
  progress: number;
  status: string;
  onStop: () => void;
}) {
  const pct = Math.max(4, Math.min(100, progress * 100));
  const label =
    status === "queued"
      ? "Waiting for a free slot…"
      : kind === "video"
        ? "Making the clip…"
        : "Drawing…";
  return (
    <div className="msg assistant">
      <div className="who">{kind === "video" ? "Video" : "Picture"}</div>
      <div
        className={`media-cup${kind === "image" ? " square" : ""}`}
        role="progressbar"
        aria-valuenow={Math.round(pct)}
        aria-label={label}
      >
        <div className="media-cup-water" style={{ height: `${pct}%` }} />
        <div className="media-cup-label">{label}</div>
      </div>
      <div className="picture-actions">
        <button className="btn sm ghost" onClick={onStop}>
          Stop
        </button>
      </div>
    </div>
  );
}

function Welcome({ noun }: { noun: string }) {
  return (
    <div style={{ textAlign: "center", padding: "60px 20px" }}>
      <div className="boot-mark" style={{ marginBottom: 18 }}>
        <Glider size={42} />
      </div>
      <h2 style={{ fontSize: 19, margin: "0 0 6px", letterSpacing: "-0.01em" }}>
        Describe your {noun}.
      </h2>
    </div>
  );
}
