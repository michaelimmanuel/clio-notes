// What a meeting used per role, as stored with it (`usage` in the timeline).
// Mirrors MeetingUsage in crates/pipeline/src/resilient.rs.

export interface RoleUsage {
  calls: number;
  failed: number;
  retries: number;
  input_tokens: number;
  output_tokens: number;
  audio_ms: number;
}

export interface MeetingUsage {
  speech_to_text: RoleUsage;
  vision: RoleUsage;
  text: RoleUsage;
  final: RoleUsage;
}

const LABELS: [keyof MeetingUsage, string][] = [
  ["speech_to_text", "Speech to text"],
  ["vision", "Screen reading"],
  ["text", "Live notes"],
  ["final", "Final notes"],
];

function count(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 10_000) return `${Math.round(n / 1000)}k`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

/** One line per role that was used, such as "Screen reading: 7 calls, 12k in / 1.2k out tokens". */
export function usageLines(u: MeetingUsage | null | undefined): { role: string; text: string }[] {
  if (!u) return [];
  const out: { role: string; text: string }[] = [];
  for (const [key, role] of LABELS) {
    const r = u[key];
    if (!r || (r.calls === 0 && r.failed === 0)) continue;
    const parts = [`${r.calls} call${r.calls === 1 ? "" : "s"}`];
    if (r.audio_ms > 0) parts.push(`${(r.audio_ms / 60000).toFixed(1)} min of audio`);
    if (r.input_tokens + r.output_tokens > 0) {
      parts.push(`${count(r.input_tokens)} in / ${count(r.output_tokens)} out tokens`);
    }
    if (r.failed > 0) parts.push(`${r.failed} failed`);
    if (r.retries > 0) parts.push(`${r.retries} retr${r.retries === 1 ? "y" : "ies"}`);
    out.push({ role, text: parts.join(", ") });
  }
  return out;
}
