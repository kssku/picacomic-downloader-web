import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import 'virtual:uno.css'
import { cssVarsToCss } from './design-tokens'

// 把 design-tokens 的 CSS 变量注入 :root。
// 修复：FloatLabelInput.vue 消费的 var(--primary-color) 此前无定义处（悬空引用）。
const styleEl = document.createElement('style')
styleEl.setAttribute('data-source', 'design-tokens')
styleEl.textContent = cssVarsToCss(':root')
document.head.appendChild(styleEl)

const pinia = createPinia()
const app = createApp(App)

app.use(pinia).mount('#app')
