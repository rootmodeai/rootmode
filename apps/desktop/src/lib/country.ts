/**
 * Turning a declared country code into something readable.
 *
 * The code is what the worker's operator typed — nothing here looks an address
 * up in a geolocation service, because doing that would hand a third party the
 * list of peers you talk to. So it is a claim, shown as one: the peers screen
 * says "says it is in" rather than "is in".
 */

/** 🇩🇪 from "DE". Regional indicator symbols are 'A' offset to U+1F1E6. */
export function flagOf(code: string | null | undefined): string {
  const cc = (code ?? "").trim().toUpperCase();
  if (!/^[A-Z]{2}$/.test(cc)) return "";
  return String.fromCodePoint(
    ...[...cc].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65),
  );
}

/** "Germany" from "DE", falling back to the code itself. */
export function countryName(code: string | null | undefined): string {
  const cc = (code ?? "").trim().toUpperCase();
  if (!/^[A-Z]{2}$/.test(cc)) return "";
  try {
    const names = new Intl.DisplayNames(undefined, { type: "region" });
    return names.of(cc) ?? cc;
  } catch {
    return cc;
  }
}

/** What to show beside a peer's name: "🇩🇪 Germany", or nothing at all. */
export function countryLabel(code: string | null | undefined): string {
  const name = countryName(code);
  if (!name) return "";
  const flag = flagOf(code);
  return flag ? `${flag} ${name}` : name;
}
