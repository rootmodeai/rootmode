import { useEffect, useState } from "react";

/**
 * The glider — the pattern from Conway's Game of Life that Eric Raymond
 * proposed in 2003 as the hacker emblem.
 *
 *   . ■ .
 *   . . ■
 *   ■ ■ ■
 *
 * Still, it is the mark. Set `animate` and it runs the actual rule: four
 * generations to a cycle, walking one cell down and right each time round,
 * drifting back by exactly that cell so it travels for ever inside its own
 * box — the rule being applied, rather than a logo spun on a turntable.
 *
 * Flat `currentColor`, no glow, no fade: generations step. One copy works on
 * light and dark alike, and at 13px as well as at 30.
 */

/** The five live cells of each generation, as (column, row) on a 4×4 grid. */
const PHASES: Array<Array<[number, number]>> = [
  [
    [1, 0],
    [2, 1],
    [0, 2],
    [1, 2],
    [2, 2],
  ],
  [
    [0, 1],
    [2, 1],
    [1, 2],
    [2, 2],
    [1, 3],
  ],
  [
    [2, 1],
    [0, 2],
    [2, 2],
    [1, 3],
    [2, 3],
  ],
  [
    [1, 1],
    [2, 2],
    [3, 2],
    [1, 3],
    [2, 3],
  ],
];

const GRID = 4;
/// Sized so the squares read as squares rather than as blobs — at 13px the
/// shape is all there is, so it has to survive being small.
const PITCH = 5;
const SIDE = 4;
const RADIUS = 0.8;
/// Still mark is generation 0 (3×3). Live mark needs the 4×4 the rule walks.
/// Both are inset so the pattern sits in the middle of the 24-unit box —
/// without that the cells hug the top-left and the logo looks off-centre.
const stillInset = (24 - (2 * PITCH + SIDE)) / 2;
const liveInset = (24 - (3 * PITCH + SIDE)) / 2;
const CYCLE = "1.8s";
/// No fade between generations. A cell is alive or it isn't — the rule steps,
/// so the mark steps, and the shape stays as sharp at 13px as at 30.

const alive = (phase: number, c: number, r: number) =>
  PHASES[phase].some(([pc, pr]) => pc === c && pr === r);

export function Glider({
  size = 24,
  className,
  animate = false,
}: {
  size?: number;
  className?: string;
  animate?: boolean;
}) {
  // SMIL has no CSS switch, so the preference is read here and the mark is
  // simply drawn still for anybody who asked for less motion.
  const still = useReducedMotion();
  const running = animate && !still;
  const inset = running ? liveInset : stillInset;

  // Every square the pattern ever touches, drawn once and switched on for the
  // generations it belongs to.
  const positions: Array<[number, number]> = [];
  for (let r = 0; r < GRID; r++) {
    for (let c = 0; c < GRID; c++) {
      if ([0, 1, 2, 3].some((p) => alive(p, c, r))) positions.push([c, r]);
    }
  }

  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      className={`glider${running ? " glider-live" : ""}${className ? ` ${className}` : ""}`}
      role="img"
      aria-label="rootmode"
    >
      {!running ? (
        PHASES[0].map(([c, r]) => (
          <rect
            key={`${c}-${r}`}
            x={inset + c * PITCH}
            y={inset + r * PITCH}
            width={SIDE}
            height={SIDE}
            rx={RADIUS}
          />
        ))
      ) : (
        <g>
          {/* The drift cancels the cell the pattern gains each cycle, so it
                travels for ever without leaving the box. */}
          <animateTransform
            attributeName="transform"
            type="translate"
            from="0 0"
            to={`${-PITCH} ${-PITCH}`}
            dur={CYCLE}
            repeatCount="indefinite"
          />
          {positions.map(([c, r]) => {
            const values = [0, 1, 2, 3].map((p) => (alive(p, c, r) ? 1 : 0));
            return (
              <rect
                key={`${c}-${r}`}
                x={inset + c * PITCH}
                y={inset + r * PITCH}
                width={SIDE}
                height={SIDE}
                rx={RADIUS}
                opacity={0}
              >
                <animate
                  attributeName="opacity"
                  values={`${values.join(";")};${values[0]}`}
                  keyTimes="0;0.25;0.5;0.75;1"
                  dur={CYCLE}
                  calcMode="discrete"
                  repeatCount="indefinite"
                />
              </rect>
            );
          })}
        </g>
      )}
    </svg>
  );
}

function useReducedMotion() {
  const [reduced, setReduced] = useState(
    () =>
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false,
  );
  useEffect(() => {
    const mq = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    if (!mq) return;
    const on = () => setReduced(mq.matches);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, []);
  return reduced;
}
