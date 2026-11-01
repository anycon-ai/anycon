import { createFileRoute } from '@tanstack/react-router'
import { FilesPage } from '@/pages/FilesPage'

// 路径就是 `fs.list` / `fs.read` 的 `path`（`<项目名>/<相对路径>`）；空 = 项目列表。splat 参数已经解码。
export const Route = createFileRoute('/_app/_shell/n/$node/files/$')({
  component: function Files() {
    const { _splat } = Route.useParams()
    return <FilesPage path={_splat ?? ''} />
  },
})
