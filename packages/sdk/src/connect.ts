import { Client, type ClientOptions } from './client.js'
import type { Node, NodeOptions } from './node.js'

export interface ConnectOptions extends ClientOptions {
  /** 节点名或 id 前缀；只配对了一个节点时可省略。 */
  node?: string
}

/**
 * 连接 hub 并打开一个已配对的节点。返回的 `Node` 拥有底层连接：`node.close()` 会一并关闭。
 * 一个进程要同时连多个节点时用 `Client.open()` + 多次 `client.node()`（hub 对单设备只允许一条连接）。
 *
 * - 没有本地身份 / token 失效 → `PairingRequired`
 * - 设备被吊销、Noise 握手被拒 → `NodeRejected`
 * - 节点不在线：照常返回，`node.online === false`，调用抛 `NodeOffline`，`node.waitOnline()` 等上线
 */
export async function connect(hubUrl: string, options: ConnectOptions = {}): Promise<Node> {
  const { node: selector, ...clientOptions } = options
  const client = await Client.open(hubUrl, clientOptions)
  try {
    const nodeOptions: NodeOptions = {}
    const node = await client.node(selector, nodeOptions)
    node.markOwnsClient()
    return node
  } catch (err) {
    await client.close()
    throw err
  }
}
