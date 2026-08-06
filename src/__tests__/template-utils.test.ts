import { describe, expect, it } from "vitest";
import { filterTemplates, templateFrameworks } from "../template-utils";
import type { TemplateInfo } from "../types";

const sample: TemplateInfo[] = [
  { id: "langchain/default", name: "Zulu", framework: "langchain", description: "LangChain default agent." },
  { id: "bub/default", name: "Alpha", framework: "bub", description: "Lightweight Bub agent." },
  { id: "langchain/agentic-rag", name: "Beta", framework: "langchain", description: "Agentic RAG with vector search." },
  { id: "deepagents/default", name: "Gamma", framework: "deepagents", description: "Local deep agent." },
];

describe("templateFrameworks", () => {
  it("returns distinct frameworks sorted alphabetically", () => {
    expect(templateFrameworks(sample)).toEqual(["bub", "deepagents", "langchain"]);
  });

  it("returns an empty list for no templates", () => {
    expect(templateFrameworks([])).toEqual([]);
  });
});

describe("filterTemplates", () => {
  it("shows everything and sorts by framework then name on the all tab", () => {
    const result = filterTemplates(sample, "all", "");
    expect(result.map((template) => template.id)).toEqual([
      "bub/default",
      "deepagents/default",
      "langchain/agentic-rag",
      "langchain/default",
    ]);
  });

  it("only returns templates of the selected framework tab", () => {
    const result = filterTemplates(sample, "langchain", "");
    expect(result.map((template) => template.id)).toEqual(["langchain/agentic-rag", "langchain/default"]);
  });

  it("combines tab filtering with a free-text query", () => {
    const result = filterTemplates(sample, "langchain", "rag");
    expect(result.map((template) => template.id)).toEqual(["langchain/agentic-rag"]);
  });

  it("searches across frameworks on the all tab", () => {
    const result = filterTemplates(sample, "all", "default");
    expect(result.map((template) => template.id)).toEqual(["bub/default", "deepagents/default", "langchain/default"]);
  });

  it("matches the framework name itself", () => {
    const result = filterTemplates(sample, "all", "deepagents");
    expect(result.map((template) => template.id)).toEqual(["deepagents/default"]);
  });

  it("returns an empty list when the tab has no templates", () => {
    expect(filterTemplates(sample, "unknown-framework", "")).toEqual([]);
  });

  it("returns an empty list when the query matches nothing", () => {
    expect(filterTemplates(sample, "all", "no-such-template")).toEqual([]);
  });

  it("ignores surrounding whitespace and case in the query", () => {
    const result = filterTemplates(sample, "all", "  LANGCHAIN/DEFAULT  ");
    expect(result.map((template) => template.id)).toEqual(["langchain/default"]);
  });
});
