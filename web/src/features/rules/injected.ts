// 会话的 `system_prompt_append` 里注入的规则（docs/spec/daemon.md § 23.5）：daemon 把规则文字放在最前面，调用方自己的那一段空一行接在后面。
// 规则文字以固定的开头一句起头；规则里的空行都缩进了两格（不会出现连续两个换行），所以第一个空行就是规则和后面那一段的分界。

/** 注入文字的开头（和 daemon 的 `TEMPLATE_HEADER` 一样）。 */
export const RULES_HEADER = 'Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:'

/** 会话设置里注入的规则文字（只读展示用）；没有注入就是 null。 */
export function injectedRulesOf(append: string | null | undefined): string | null {
  if (!append || !append.startsWith(RULES_HEADER)) return null
  const end = append.indexOf('\n\n')
  return end < 0 ? append : append.slice(0, end)
}

/** 注入的规则条数：行首的 `- `。 */
export const injectedRuleCount = (text: string): number => (text.match(/^- /gm) ?? []).length
