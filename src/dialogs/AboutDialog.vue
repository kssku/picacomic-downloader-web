<script setup lang="ts">
import { commands } from '../bindings.ts'
import { useStore } from '../store.ts'
import { ref, onMounted } from 'vue'
import icon from '/favicon.png'

const store = useStore()

const showing = defineModel<boolean>('showing', { required: true })
const version = ref('')

onMounted(async () => {
  if (store.serverInfo === undefined) {
    store.serverInfo = (await commands.getServerInfo()) ?? undefined
  }
  version.value = store.serverInfo?.version ?? ''
})
</script>

<template>
  <n-modal v-model:show="showing">
    <n-dialog :showIcon="false" @close="showing = false">
      <div class="flex flex-col items-center gap-row-6">
        <img :src="icon" alt="icon" class="w-32 h-32" />
        <div class="text-center text-gray-400 text-xs">
          <div>
            如果本项目对你有帮助，欢迎来
            <n-a href="https://github.com/lanyeeee/picacomic-downloader" target="_blank">GitHub</n-a>
            点个Star⭐支持！
          </div>
          <div class="mt-1">你的支持是我持续更新维护的动力🙏</div>
        </div>
        <div class="flex flex-col w-full gap-row-3 px-6">
          <div class="flex items-center justify-between py-2 px-4 bg-gray-100 rounded-lg">
            <span class="text-gray-500">软件版本</span>
            <div class="font-medium">v{{ version }}</div>
          </div>
          <div class="flex items-center justify-between py-2 px-4 bg-gray-100 rounded-lg">
            <span class="text-gray-500">开源地址</span>
            <n-a href="https://github.com/lanyeeee/picacomic-downloader" target="_blank">GitHub</n-a>
          </div>
          <div class="flex items-center justify-between py-2 px-4 bg-gray-100 rounded-lg">
            <span class="text-gray-500">问题反馈</span>
            <n-a href="https://github.com/lanyeeee/picacomic-downloader/issues" target="_blank">GitHub Issues</n-a>
          </div>
        </div>
        <div class="flex flex-col text-xs items-center text-gray-400">
          <div>
            Copyright © 2024-2025
            <n-a href="https://github.com/lanyeeee" target="_blank">lanyeeee</n-a>
          </div>
          <div>
            Released under
            <n-a href="https://github.com/lanyeeee/picacomic-downloader/blob/main/LICENSE" target="_blank">
              MIT License
            </n-a>
          </div>
        </div>
      </div>
    </n-dialog>
  </n-modal>
</template>
