import type { TemplateInfo } from "./types";

/**
 * Distinct template types (framework = first segment of the template id),
 * sorted alphabetically. Rendered as tabs above the template list.
 */
export function templateFrameworks(templates: TemplateInfo[]): string[] {
  const seen: string[] = [];
  for (const template of templates) {
    if (!seen.includes(template.framework)) seen.push(template.framework);
  }
  return seen.sort();
}

/**
 * Filter templates by type tab ("all" shows every framework) plus an
 * optional free-text query, then sort by framework and name.
 */
export function filterTemplates(
  templates: TemplateInfo[],
  tab: string,
  query: string,
): TemplateInfo[] {
  const trimmed = query.trim().toLowerCase();
  return templates
    .filter(
      (template) =>
        (tab === "all" || template.framework === tab) &&
        (!trimmed ||
          [template.id, template.name, template.description, template.framework].some((value) =>
            value.toLowerCase().includes(trimmed),
          )),
    )
    .sort((a, b) => {
      const frameworkCmp = a.framework.localeCompare(b.framework);
      if (frameworkCmp !== 0) return frameworkCmp;
      return a.name.localeCompare(b.name);
    });
}
