// Approximate per-model metadata for the status bar's context-window % and
// running cost. Catalogs like this are inherently approximate and need upkeep;
// unknown models simply show tokens with no % or cost. Prices are USD per
// million tokens (input / output).

export interface ModelInfo {
  contextWindow: number;
  inputPerMTok: number;
  outputPerMTok: number;
}

// First matching pattern wins (most specific first).
const TABLE: Array<[RegExp, ModelInfo]> = [
  [/claude.*opus/, { contextWindow: 200_000, inputPerMTok: 15, outputPerMTok: 75 }],
  [/claude.*haiku/, { contextWindow: 200_000, inputPerMTok: 1, outputPerMTok: 5 }],
  [/claude|sonnet/, { contextWindow: 200_000, inputPerMTok: 3, outputPerMTok: 15 }],
  [/gpt-?5/, { contextWindow: 400_000, inputPerMTok: 1.25, outputPerMTok: 10 }],
  [/o3|gpt-4\.1|gpt-4o/, { contextWindow: 128_000, inputPerMTok: 2.5, outputPerMTok: 10 }],
  [/grok-4|grok-3|composer/, { contextWindow: 256_000, inputPerMTok: 2, outputPerMTok: 10 }],
  [/grok/, { contextWindow: 131_072, inputPerMTok: 2, outputPerMTok: 10 }],
];

/** Approximate metadata for a model id, or undefined when unknown. */
export function modelInfo(model: string): ModelInfo | undefined {
  const id = model.toLowerCase();
  for (const [pattern, info] of TABLE) if (pattern.test(id)) return info;
  return undefined;
}
