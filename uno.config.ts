import {
    defineConfig,
    presetAttributify,
    presetIcons,
    presetTypography,
    presetUno,
    presetWebFonts,
    transformerDirectives,
    transformerVariantGroup
} from 'unocss'
import { palette } from './src/design-tokens'

export default defineConfig({
    shortcuts: [
        // ...
    ],
    // 扫描范围：补上 .ts。
    //
    // 原因：UnoCSS 的 defaultPipelineInclude 是
    //   /\.(vue|svelte|[jt]sx|mdx?|astro|elm|php|phtml|html)($|\?)/
    // —— 只有 [jt]sx，没有独立的 ts，所以 .ts 文件整体不被扫描。
    // 而 .vue 虽然被扫，用的却是「按类名语法提取」的 extractor：
    // 它认 class="..." 属性里的类名，不认 <script> 里 return '...' 的孤立字符串。
    //
    // 结果：写在 .ts 或 .vue <script> 里的 arbitrary class 字面量
    // （如 STATE_TO_TEXT_CLASS 的 text-[var(--state-*)]）扫不到，CSS 不生成。
    //
    // 代价：扫到不认识的字符串不会生成 CSS，只是多花一点扫描时间；
    // 唯一的副作用是误匹配时多出几十字节用不到的 CSS。
    content: {
        pipeline: {
            include: [/\.(vue|svelte|[jt]sx|mdx?|astro|elm|php|phtml|html|ts)($|\?)/],
        },
    },
    theme: {
        colors: {
            // 唯一来源：src/design-tokens.ts
            // 不要在这里写字面量色值 —— 否则又会和 themeOverrides 分裂。
            primary: {
                DEFAULT: palette.primary.DEFAULT,
                hover: palette.primary.hover,
                pressed: palette.primary.pressed,
                suppl: palette.primary.suppl,
            },
        }
    },
    presets: [
        presetUno(),
        presetAttributify(),
        presetIcons(),
        presetTypography(),
        presetWebFonts({
            fonts: {
                // ...
            },
        }),
    ],
    transformers: [
        transformerDirectives(),
        transformerVariantGroup(),
    ],
})
