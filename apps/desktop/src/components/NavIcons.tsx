/**
 * The rail's icons. Plain stroked shapes — same weight and corner
 * treatment as the marketing site's feature icons — so the sidebar reads as
 * drawn by the same hand as the rest of the app, not borrowed from a font.
 */

type Props = { size?: number };

const base = {
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 2,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

export function ChatIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <path d="M21 15a2 2 0 0 1-2 2H8l-5 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
    </svg>
  );
}

export function ImagesIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <rect x="3" y="3" width="18" height="18" rx="2" />
      <circle cx="8.5" cy="8.5" r="1.5" />
      <path d="M21 15l-5-5L6 20" />
    </svg>
  );
}

export function VideoIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <rect x="2" y="5" width="14" height="14" rx="2" />
      <path d="M16 10l6-4v12l-6-4z" />
    </svg>
  );
}

export function FlowsIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <rect x="2" y="3" width="6" height="6" rx="1.5" />
      <rect x="16" y="9" width="6" height="6" rx="1.5" />
      <rect x="2" y="15" width="6" height="6" rx="1.5" />
      <path d="M8 6h3a3 3 0 013 3v0M8 18h3a3 3 0 003-3v0M14 12h2" />
    </svg>
  );
}

export function CreateIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <path d="M12 3l2.1 4.9L19 10l-4.9 2.1L12 17l-2.1-4.9L5 10l4.9-2.1z" />
      <path d="M18 15.5l.9 2.1 2.1.9-2.1.9-.9 2.1-.9-2.1-2.1-.9 2.1-.9z" />
    </svg>
  );
}

export function ProvidersIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <circle cx="18" cy="5" r="2.5" />
      <circle cx="6" cy="12" r="2.5" />
      <circle cx="18" cy="19" r="2.5" />
      <path d="M8.3 10.6l7.4-4.2M8.3 13.4l7.4 4.2" />
    </svg>
  );
}

export function ConnectIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <path d="M17 3l4 4-4 4" />
      <path d="M3 7h18" />
      <path d="M7 21l-4-4 4-4" />
      <path d="M21 17H3" />
    </svg>
  );
}

export function WalletIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <rect x="2" y="6" width="20" height="14" rx="2" />
      <path d="M2 10h20" />
      <circle cx="16" cy="15" r="1.2" />
    </svg>
  );
}

export function SettingsIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </svg>
  );
}

export function SkullIcon({ size = 16 }: Props) {
  return (
    <svg width={size} height={size} {...base} aria-hidden="true">
      <path d="M12 2c-4.4 0-8 3.2-8 7.2 0 2.4 1.2 4.1 2.7 5.2.5.4.8.9.8 1.5V18a2 2 0 0 0 2 2h5a2 2 0 0 0 2-2v-2.1c0-.6.3-1.1.8-1.5C19.8 13.3 21 11.6 21 9.2 21 5.2 16.4 2 12 2z" />
      <circle cx="9" cy="10" r="1.6" />
      <circle cx="15" cy="10" r="1.6" />
      <path d="M12 13.5v2.5M10 20v-2.5M14 20v-2.5" />
    </svg>
  );
}
