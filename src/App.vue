<script setup lang="ts">
// This starter template is using Vue 3 <script setup> SFCs
// Check out https://vuejs.org/api/sfc-script-setup.html#script-setup
import AppContent from './AppContent.vue'
import { GlobalThemeOverrides } from 'naive-ui'
import { palette, radii } from './design-tokens'

// 全部色值 / 圆角来自 design-tokens.ts —— 不要在组件里写字面量。
const themeOverrides: GlobalThemeOverrides = {
  common: {
    primaryColor: palette.primary.DEFAULT,
    primaryColorHover: palette.primary.hover,
    primaryColorPressed: palette.primary.pressed,
    primaryColorSuppl: palette.primary.suppl,
    borderRadius: radii.md,
    borderRadiusSmall: radii.sm,
    heightMedium: '32px',
  },
  Button: {
    paddingSmall: '0 8px',
    paddingMedium: '0 12px',
  },
  Radio: {
    buttonColorActive: palette.primary.DEFAULT,
    buttonTextColorActive: palette.neutral.onPrimary,
  },
  Dropdown: {
    borderRadius: '5px',
    padding: '6px 2px',
    optionColorHover: palette.primary.DEFAULT,
    optionTextColorHover: palette.neutral.onPrimary,
    optionHeightMedium: '28px',
  },
  Tabs: {
    tabTextColorActiveSegment: palette.primary.DEFAULT,
    tabTextColorHoverSegment: palette.primary.hover,
    tabColorSegment: palette.primary.soft,
    colorSegment: '#FFFFFFFF',
  },
}
</script>

<template>
  <n-config-provider :theme-overrides="themeOverrides">
    <n-modal-provider>
      <n-notification-provider placement="bottom-right" :max="3">
        <n-message-provider>
          <app-content />
        </n-message-provider>
      </n-notification-provider>
    </n-modal-provider>
  </n-config-provider>
</template>
