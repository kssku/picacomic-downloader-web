/**
 * 设计 token 单一来源（picacomic-downloader-web）
 *
 * 注意：本文件会被 UnoCSS 扫描（uno.config.ts 的 content.pipeline.include
 * 已补上 `.ts`）。因此：
 *   - 这里出现的 **完整 arbitrary class 字面量**（如 bg-[var(--x)]）会被扫到并生成 CSS；
 *   - 但拼接出来的类名（模板字符串 / 变量拼接）扫不到，必须在消费端写完整字面量。
 * 颜色只在这里定义一次，App.vue 的 themeOverrides 与 uno.config.ts 都从这里读。
 */

/** 主色：picacomic 的视觉识别色（玫红），不随其他项目统一 */
export const palette = {
  primary: {
    DEFAULT: '#DB547C',
    hover: '#E87D9A',
    pressed: '#B53C64',
    suppl: '#E87D9A',
    /** Tabs 选中底色（淡粉） */
    soft: '#FFDAE9',
  },
  state: {
    /** 已下载 / 成功 */
    success: 'rgba(24,160,88,0.16)',
    /** 下载中 */
    downloading: 'rgba(114,46,209,0.16)',
  },
  neutral: {
    /** 选中项底色（淡蓝） */
    selected: 'rgb(204,232,255)',
    /** 分割线 */
    border: 'rgb(239,239,245)',
    /** 框选区域 */
    selection: 'rgba(46,115,252,0.5)',
    /** 反色文字 */
    onPrimary: '#FFF',
  },
} as const

/** 圆角：与 themeOverrides 中的 borderRadius 保持一致 */
export const radii = {
  md: '4px',
  sm: '3px',
} as const

/**
 * 注入到 :root 的 CSS 变量。只导出**实际有消费端**的变量，
 * 不照搬 jmcomic 的 alpha 档位 / 玻璃模糊（本项目管理端不存在）。
 */
export const cssVars: Record<string, string> = {
  '--primary-color': palette.primary.DEFAULT,
  '--primary-color-hover': palette.primary.hover,
  '--primary-color-pressed': palette.primary.pressed,
  '--primary-color-soft': palette.primary.soft,
  '--state-success': palette.state.success,
  '--state-downloading': palette.state.downloading,
  '--neutral-selected': palette.neutral.selected,
  '--neutral-border': palette.neutral.border,
  '--neutral-selection': palette.neutral.selection,
  '--text-on-primary': palette.neutral.onPrimary,
  '--radius-md': radii.md,
  '--radius-sm': radii.sm,
}

export function cssVarsToCss(selector = ':root'): string {
  const body = Object.entries(cssVars)
    .map(([k, v]) => `  ${k}: ${v};`)
    .join('\n')
  return `${selector} {\n${body}\n}\n`
}
