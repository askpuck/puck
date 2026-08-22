import { useEffect, useRef } from "react";
import { CREW, RoleId } from "./crew";

/**
 * Formine — one silhouette per trade, one colour each. No faces, no second fill.
 *
 * Coordinator     upright figure     sage      the person you talk to
 * Coder            rounded brick      clay      builds, stacked, made
 */

export const ROLE_COLOR: Record<RoleId, string> = {
  coordinatore: "#6FA37C",
  coder: "#C17A58",
};

function Body({ id }: { id: RoleId }) {
  const c = ROLE_COLOR[id];
  switch (id) {
    case "coordinatore":
      return (
        <path
          fill={c}
          d="M16 3.6c3.6 0 6.4 2.8 6.4 6.6 0 2.2-1.1 4.1-2.8 5.3 3.4 1.2 5.8 4.4 5.8 8.1 0 3.4-3.8 5.8-9.4 5.8s-9.4-2.4-9.4-5.8c0-3.7 2.4-6.9 5.8-8.1A6.4 6.4 0 0 1 9.6 10.2C9.6 6.4 12.4 3.6 16 3.6Z"
        />
      );
    case "coder":
      return <rect x="5.5" y="5.5" width="21" height="21" rx="7.2" fill={c} />;
  }
}

export function Formina({ id, size = 32 }: { id: RoleId; size?: number }) {
  return (
    <svg
      className="formina"
      width={size}
      height={size}
      viewBox="0 0 32 32"
      aria-hidden
    >
      <Body id={id} />
    </svg>
  );
}

function crewWho(who: RoleId[]): RoleId[] {
  const set = new Set(who);
  const out: RoleId[] = [];
  if (set.has("coordinatore")) out.push("coordinatore");
  for (const role of CREW) {
    if (role.id === "coordinatore" || !set.has(role.id)) continue;
    out.push(role.id);
  }
  return out.length ? out : ["coordinatore"];
}

export function CrewSpin({ who, size = 24 }: { who: RoleId[]; size?: number }) {
  const ids = crewWho(who);
  const root = useRef<HTMLSpanElement>(null);
  const key = ids.join(",");
  useEffect(() => {
    const el = root.current;
    if (!el || ids.length < 2) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const layers = [...el.querySelectorAll<HTMLElement>(":scope > span")];
    const n = layers.length;
    const slot = 2500;
    const fade = 1000;
    const total = n * slot;
    const frames: Keyframe[] = [
      { offset: 0, opacity: 0 },
      { offset: fade / total, opacity: 1 },
      { offset: slot / total, opacity: 1 },
      { offset: (slot + fade) / total, opacity: 0 },
      { offset: 1, opacity: 0 },
    ];
    const anims = layers.map((layer, i) =>
      layer.animate(frames, {
        duration: total,
        delay: i * slot - fade,
        iterations: Infinity,
        easing: "linear",
        fill: "both",
      }),
    );
    return () => anims.forEach((a) => a.cancel());
  }, [key, ids.length]);
  return (
    <span
      ref={root}
      className={ids.length < 2 ? "crew-spin is-one" : "crew-spin"}
      style={{ width: size, height: size }}
      aria-hidden
    >
      {ids.map((id) => (
        <span key={id}>
          <Formina id={id} size={size} />
        </span>
      ))}
    </span>
  );
}
