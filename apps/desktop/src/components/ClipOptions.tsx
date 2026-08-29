import type { VideoOffer } from "../lib/types";
import { quote, shapeOf, soundIsAChoice, type ClipChoice } from "../lib/video";

/**
 * The knobs a video model actually has — no more. A model with one
 * resolution shows no resolution picker; one that cannot be silenced shows
 * no sound switch. The price beside them is the quote for what is chosen,
 * which is what will be locked and billed.
 */
export function ClipOptions({
  offer,
  choice,
  onChange,
  currency,
  disabled,
  compact,
}: {
  offer: VideoOffer;
  choice: ClipChoice;
  onChange: (c: ClipChoice) => void;
  currency: string;
  disabled?: boolean;
  compact?: boolean;
}) {
  const shape = shapeOf(offer, choice);
  const price = quote(offer, choice);
  const set = (patch: ClipChoice) => onChange({ ...choice, ...patch });
  const seconds = offer.durations.length > 0 ? offer.durations : [offer.default_seconds];
  return (
    <div className={`clip-opts${compact ? " compact" : ""}`}>
      {seconds.length > 1 && (
        <label>
          <select value={shape.seconds} onChange={(e) => set({ seconds: Number(e.target.value) })} disabled={disabled} title="How long">
            {seconds.map((s) => (
              <option key={s} value={s}>{s} s</option>
            ))}
          </select>
        </label>
      )}
      {offer.resolutions.length > 1 && (
        <label>
          <select value={shape.resolution ?? ""} onChange={(e) => set({ resolution: e.target.value })} disabled={disabled} title="Resolution">
            {offer.resolutions.map((r) => (
              <option key={r} value={r}>{r}</option>
            ))}
          </select>
        </label>
      )}
      {offer.aspect_ratios.length > 1 && (
        <label>
          <select value={shape.aspect_ratio ?? ""} onChange={(e) => set({ aspect_ratio: e.target.value })} disabled={disabled} title="Aspect ratio">
            {offer.aspect_ratios.map((a) => (
              <option key={a} value={a}>{a}</option>
            ))}
          </select>
        </label>
      )}
      {soundIsAChoice(offer.audio) && (
        <label className="check" title="Sound costs more on this model">
          <input type="checkbox" checked={shape.audio} onChange={(e) => set({ audio: e.target.checked })} disabled={disabled} />
          <span>sound</span>
        </label>
      )}
      {seconds.length <= 1 && offer.resolutions.length <= 1 && offer.aspect_ratios.length <= 1 && !soundIsAChoice(offer.audio) && (
        <span className="fixed">{shape.seconds} s{shape.resolution ? ` · ${shape.resolution}` : ""}{shape.aspect_ratio ? ` · ${shape.aspect_ratio}` : ""}</span>
      )}
      {price !== null && (
        <span className="price">
          {price.toFixed(2)} {currency}
        </span>
      )}
    </div>
  );
}
