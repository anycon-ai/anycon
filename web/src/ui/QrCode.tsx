import { useMemo } from 'react'
import { renderSVG } from 'uqr'

/** 二维码（uqr 生成 SVG）。深浅主题下都用白底黑块，保证可扫。 */
export function QrCode({ text, size = 176, label }: { text: string; size?: number; label: string }) {
  const svg = useMemo(() => renderSVG(text, { ecc: 'M', border: 2, whiteColor: '#ffffff', blackColor: '#000000' }), [text])
  return (
    <div
      role="img"
      aria-label={label}
      className="inline-block overflow-hidden rounded-md border border-border bg-white"
      style={{ width: size, height: size }}
      // uqr 输出的是自己生成的纯 SVG，没有用户可控内容
      dangerouslySetInnerHTML={{ __html: svg.replace('<svg ', `<svg width="${size}" height="${size}" `) }}
    />
  )
}
