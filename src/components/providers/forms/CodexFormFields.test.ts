import { describe, expect, it } from "vitest";
import {
  applyCodexCustomModelCatalogSelection,
  buildCustomModelAdditionsFromFetched,
  customRowsMatchModels,
  groupCodexCustomModelsByProvider,
  mergeProviderRouteModelOptions,
} from "./CodexFormFields";

describe("applyCodexCustomModelCatalogSelection", () => {
  it("refreshes or clears every hidden capability field with the upstream model", () => {
    const previous = {
      model: "gpt-5.2",
      providerId: "provider-1",
      upstreamModel: "old-model",
      displayName: "Old Model",
      contextWindow: 64_000,
      supportsParallelToolCalls: false,
      inputModalities: ["text"],
      baseInstructions: "old instructions",
    };

    const selected = applyCodexCustomModelCatalogSelection(
      previous,
      "new-model",
      {
        model: "new-model",
        displayName: " New Model ",
        contextWindow: 256_000,
        supportsParallelToolCalls: true,
        inputModalities: ["text", "image"],
        baseInstructions: "new instructions",
      },
    );
    expect(selected).toMatchObject({
      upstreamModel: "new-model",
      displayName: "New Model",
      contextWindow: 256_000,
      supportsParallelToolCalls: true,
      inputModalities: ["text", "image"],
      baseInstructions: "new instructions",
    });

    const uncatalogued = applyCodexCustomModelCatalogSelection(
      selected,
      "uncatalogued-model",
      undefined,
    );
    expect(uncatalogued.displayName).toBe("");
    expect(uncatalogued.contextWindow).toBe("");
    expect(uncatalogued.supportsParallelToolCalls).toBeUndefined();
    expect(uncatalogued.inputModalities).toBeUndefined();
    expect(uncatalogued.baseInstructions).toBeUndefined();
  });
});

describe("customRowsMatchModels", () => {
  it("detects changes to hidden native model profile fields", () => {
    const visibleFields = {
      model: "gpt-5.2",
      providerId: "provider-1",
      upstreamModel: "vendor-model",
      displayName: "Vendor Model",
      contextWindow: 128_000,
    };

    expect(
      customRowsMatchModels(
        [
          {
            ...visibleFields,
            supportsParallelToolCalls: false,
            inputModalities: ["text"],
            baseInstructions: "old instructions",
          },
        ],
        [
          {
            ...visibleFields,
            supportsParallelToolCalls: true,
            inputModalities: ["text", "image"],
            baseInstructions: "new instructions",
          },
        ],
      ),
    ).toBe(false);
  });

  it("returns true for equal rows including hidden profile fields", () => {
    const rows = [
      {
        model: "gpt-5.2",
        providerId: "provider-1",
        upstreamModel: "vendor-model",
        displayName: "Vendor Model",
        contextWindow: 128_000,
        supportsParallelToolCalls: true,
        inputModalities: ["text", "image"],
        baseInstructions: "instructions",
      },
    ];
    expect(customRowsMatchModels(rows, [...rows])).toBe(true);
  });

  it("returns false when row counts differ", () => {
    expect(
      customRowsMatchModels(
        [{ model: "gpt-5.2", providerId: "provider-1" }],
        [
          { model: "gpt-5.2", providerId: "provider-1" },
          { model: "gpt-5.4", providerId: "provider-2" },
        ],
      ),
    ).toBe(false);
  });

  it("treats undefined and empty-string optional fields as equal", () => {
    expect(
      customRowsMatchModels(
        [
          {
            model: "gpt-5.2",
            providerId: "provider-1",
            upstreamModel: undefined,
          },
        ],
        [{ model: "gpt-5.2", providerId: "provider-1", upstreamModel: "" }],
      ),
    ).toBe(true);
  });

  it("compares contextWindow across number and string representations", () => {
    expect(
      customRowsMatchModels(
        [
          {
            model: "gpt-5.2",
            providerId: "provider-1",
            contextWindow: 128_000,
          },
        ],
        [
          {
            model: "gpt-5.2",
            providerId: "provider-1",
            contextWindow: "128000",
          },
        ],
      ),
    ).toBe(true);
  });
});

describe("buildCustomModelAdditionsFromFetched", () => {
  it("adds every fetched model bound to the provider with display id defaults", () => {
    const additions = buildCustomModelAdditionsFromFetched(
      "provider-1",
      [
        { id: "model-a", ownedBy: null },
        { id: "model-b", ownedBy: null },
      ],
      [],
    );
    expect(additions).toEqual([
      {
        model: "model-a",
        providerId: "provider-1",
        upstreamModel: "model-a",
        displayName: "model-a",
        routes: [{ providerId: "provider-1", upstreamModel: "model-a" }],
      },
      {
        model: "model-b",
        providerId: "provider-1",
        upstreamModel: "model-b",
        displayName: "model-b",
        routes: [{ providerId: "provider-1", upstreamModel: "model-b" }],
      },
    ]);
  });

  it("skips models already bound to the same provider", () => {
    const existing = [
      {
        model: "model-a",
        providerId: "provider-1",
        upstreamModel: "model-a",
        routes: [{ providerId: "provider-1", upstreamModel: "model-a" }],
      },
    ];
    const additions = buildCustomModelAdditionsFromFetched(
      "provider-1",
      [
        { id: "model-a", ownedBy: null },
        { id: "model-b", ownedBy: null },
      ],
      existing,
    );
    expect(additions).toHaveLength(1);
    expect(additions[0]).toMatchObject({
      model: "model-b",
      providerId: "provider-1",
    });
  });

  it("keeps a model id that another provider already exposes", () => {
    const existing = [
      {
        model: "model-a",
        providerId: "provider-1",
        upstreamModel: "model-a",
        routes: [{ providerId: "provider-1", upstreamModel: "model-a" }],
      },
    ];
    const additions = buildCustomModelAdditionsFromFetched(
      "provider-2",
      [{ id: "model-a", ownedBy: null }],
      existing,
    );
    expect(additions).toHaveLength(1);
    expect(additions[0].providerId).toBe("provider-2");
  });
});

describe("mergeProviderRouteModelOptions", () => {
  it("groups saved catalog and fetched models under the provider name", () => {
    const options = mergeProviderRouteModelOptions(
      "DeepSeek",
      [{ model: "deepseek-chat", displayName: "DeepSeek Chat" }],
      [
        { id: "deepseek-chat", ownedBy: "deepseek" },
        { id: "deepseek-reasoner", ownedBy: "deepseek" },
      ],
    );
    expect(options).toEqual([
      { id: "deepseek-chat", ownedBy: "DeepSeek" },
      { id: "deepseek-reasoner", ownedBy: "DeepSeek" },
    ]);
  });

  it("deduplicates fetched models already present in the saved catalog", () => {
    const options = mergeProviderRouteModelOptions(
      "Kimi",
      [{ model: "kimi-k2", displayName: "Kimi K2" }],
      [{ id: "kimi-k2", ownedBy: "moonshot" }],
    );
    expect(options).toEqual([{ id: "kimi-k2", ownedBy: "Kimi" }]);
  });

  it("falls back to Catalog when the provider name is missing", () => {
    const options = mergeProviderRouteModelOptions(
      "",
      [{ model: "model-a" }],
      [{ id: "model-b", ownedBy: null }],
    );
    expect(options).toEqual([
      { id: "model-a", ownedBy: "Catalog" },
      { id: "model-b", ownedBy: "Catalog" },
    ]);
  });

  it("drops blank saved catalog entries", () => {
    const options = mergeProviderRouteModelOptions(
      "GLM",
      [{ model: "  " }, { model: "glm-5", displayName: "GLM 5" }],
      [],
    );
    expect(options).toEqual([{ id: "glm-5", ownedBy: "GLM" }]);
  });
});

describe("groupCodexCustomModelsByProvider", () => {
  it("groups independent model mappings by primary provider in first-seen order", () => {
    const rows = [
      {
        model: "deepseek-a",
        routes: [{ providerId: "deepseek", upstreamModel: "deepseek-a" }],
      },
      {
        model: "glm-a",
        routes: [{ providerId: "glm", upstreamModel: "glm-a" }],
      },
      {
        model: "deepseek-b",
        routes: [{ providerId: "deepseek", upstreamModel: "deepseek-b" }],
      },
    ];

    const groups = groupCodexCustomModelsByProvider(rows);

    expect(groups.map((group) => group.providerId)).toEqual([
      "deepseek",
      "glm",
    ]);
    expect(groups[0].rows.map(({ row }) => row.model)).toEqual([
      "deepseek-a",
      "deepseek-b",
    ]);
    expect(groups[1].rows.map(({ index }) => index)).toEqual([1]);
  });

  it("keeps blank provider drafts as separate supplier groups", () => {
    const groups = groupCodexCustomModelsByProvider([
      { model: "", routes: [{ providerId: "" }] },
      { model: "", routes: [{ providerId: "" }] },
    ]);

    expect(groups).toHaveLength(2);
    expect(groups[0].key).not.toBe(groups[1].key);
  });
});
