// 测试里切换视口断点：`useMediaQuery` 读 `window.matchMedia`，这里按 `(min-width: Npx)` 和给定宽度算。
// 不调用的话 setup.ts 里的默认值让所有 min-width 查询都成立（桌面）。
export type Viewport = 'phone' | 'tablet' | 'desktop'

const WIDTH: Record<Viewport, number> = { phone: 390, tablet: 900, desktop: 1440 }

export function setViewport(v: Viewport): void {
  const width = WIDTH[v]
  window.matchMedia = ((query: string) => {
    const min = /min-width:\s*(\d+)px/.exec(query)
    return {
      matches: min ? width >= Number(min[1]) : false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }
  }) as unknown as typeof window.matchMedia
}

export function resetViewport(): void {
  setViewport('desktop')
}
