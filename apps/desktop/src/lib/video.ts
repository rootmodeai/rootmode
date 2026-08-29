// Shaping a clip: what the user may choose from a provider's menu, and
// what that costs — the same arithmetic the worker bills by, so the lock
// the app takes before a job is exactly the bill after it.

import type { AudioOffer, VideoOffer, VideoParams, VideoRate } from "./types";

/** What the user chose. Anything absent is the provider's default. */
export interface ClipChoice {
  seconds?: number;
  resolution?: string;
  aspect_ratio?: string;
  audio?: boolean;
}

export interface ClipShape {
  seconds: number;
  resolution: string | null;
  aspect_ratio: string | null;
  audio: boolean;
  from_image: boolean;
}

/** Round up to the cent, the way prices are quoted everywhere in the app. */
function ceilCents(x: number): number {
  return Math.ceil(x * 100 - 1e-9) / 100;
}

/** The clip a choice resolves to on this offer: choices where on the menu,
 * defaults where not. Mirrors `VideoOffer::shape_for` in rootmode-core. */
export function shapeOf(offer: VideoOffer, choice: ClipChoice, fromImage = false): ClipShape {
  const seconds =
    choice.seconds !== undefined && (offer.durations.length === 0 || offer.durations.includes(choice.seconds))
      ? choice.seconds
      : offer.default_seconds;
  const resolution =
    choice.resolution !== undefined
      ? (offer.resolutions.find((r) => r.toLowerCase() === choice.resolution!.toLowerCase()) ?? offer.default_resolution ?? null)
      : (offer.default_resolution ?? null);
  const aspect_ratio =
    choice.aspect_ratio !== undefined && offer.aspect_ratios.includes(choice.aspect_ratio)
      ? choice.aspect_ratio
      : (offer.default_aspect ?? null);
  const audio = offer.audio === "always" ? true : offer.audio === "never" ? false : (choice.audio ?? false);
  return { seconds, resolution, aspect_ratio, audio, from_image: fromImage };
}

function rateFor(offer: VideoOffer, shape: ClipShape): VideoRate | undefined {
  const fits = (r: VideoRate) =>
    r.audio === shape.audio &&
    (r.resolution == null ? true : shape.resolution != null && r.resolution.toLowerCase() === shape.resolution.toLowerCase());
  const dearest = (rs: VideoRate[]) => rs.reduce<VideoRate | undefined>((m, r) => (m && m.usd_per_second >= r.usd_per_second ? m : r), undefined);
  const exact = dearest(offer.rates.filter(fits).filter((r) => r.from_image === shape.from_image));
  return exact ?? dearest(offer.rates.filter(fits));
}

/** What a choice costs on this offer, in the offer's currency, or null
 * when there is no offer to quote from (an older provider: use its
 * flat price). */
export function quote(offer: VideoOffer | null | undefined, choice: ClipChoice, fromImage = false): number | null {
  if (!offer) return null;
  const shape = shapeOf(offer, choice, fromImage);
  const rate = rateFor(offer, shape);
  if (!rate) return null;
  return ceilCents(Math.max(rate.usd_per_second * shape.seconds, rate.minimum_usd));
}

/** Only the fields the user actually chose go on the job, so a provider
 * that was never asked keeps making exactly the clip it always did. */
export function clipParams(choice: ClipChoice): Pick<VideoParams, "seconds" | "resolution" | "aspect_ratio" | "audio"> {
  const out: Pick<VideoParams, "seconds" | "resolution" | "aspect_ratio" | "audio"> = {};
  if (choice.seconds !== undefined) out.seconds = choice.seconds;
  if (choice.resolution !== undefined) out.resolution = choice.resolution;
  if (choice.aspect_ratio !== undefined) out.aspect_ratio = choice.aspect_ratio;
  if (choice.audio !== undefined) out.audio = choice.audio;
  return out;
}

/** Drop choices that are not on this offer's menu — for when the model changes. */
export function tidy(offer: VideoOffer | null | undefined, choice: ClipChoice): ClipChoice {
  if (!offer) return {};
  const out: ClipChoice = {};
  if (choice.seconds !== undefined && (offer.durations.length === 0 ? choice.seconds === offer.default_seconds : offer.durations.includes(choice.seconds)))
    out.seconds = choice.seconds;
  if (choice.resolution !== undefined && offer.resolutions.some((r) => r.toLowerCase() === choice.resolution!.toLowerCase()))
    out.resolution = choice.resolution;
  if (choice.aspect_ratio !== undefined && offer.aspect_ratios.includes(choice.aspect_ratio)) out.aspect_ratio = choice.aspect_ratio;
  if (choice.audio !== undefined && offer.audio === "optional") out.audio = choice.audio;
  return out;
}

/** "8 s · 1080p · 9:16 · sound" */
export function clipLabel(offer: VideoOffer | null | undefined, choice: ClipChoice): string {
  if (!offer) return "5 s · 720p · 16:9";
  const s = shapeOf(offer, choice);
  return [`${s.seconds} s`, s.resolution, s.aspect_ratio, s.audio ? "sound" : null].filter(Boolean).join(" · ");
}

export function soundIsAChoice(audio: AudioOffer): boolean {
  return audio === "optional";
}
