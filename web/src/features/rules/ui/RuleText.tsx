// 规则的全文：纯文字，换行照原样，长词折行。规则的文字是用户内容，也可能是 agent 写的待确认的话：不当 Markdown、不当 HTML。
// 单独一个文件、不依赖路由：权限卡片（会话页和「待处理」里的）经「记成规则」的编辑框用到它，不能把路由拖进它们的依赖里。
export function RuleText({ text, testId = 'rule-text' }: { text: string; testId?: string }) {
  return (
    <p className="text-sm whitespace-pre-wrap [overflow-wrap:anywhere] select-text" data-testid={testId}>
      {text}
    </p>
  )
}
