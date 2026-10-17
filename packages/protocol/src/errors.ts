/** 编解码层错误。收到的字节不合法时抛出，调用方通常记日志后丢弃该帧。 */
export class ProtocolError extends Error {
  override readonly name: string = 'ProtocolError'
}
