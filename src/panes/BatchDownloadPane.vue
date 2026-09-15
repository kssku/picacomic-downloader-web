<script setup lang="ts">
import { ref } from 'vue'
import { useMessage } from 'naive-ui'
import { commands } from '../bindings.ts'

const message = useMessage()

const idsInput = ref('')
const loading = ref(false)

async function startBatchDownload() {
  // 解析ID：按行或逗号分割，去重、去空格、过滤空值
  const raw = idsInput.value
    .split(/[\n,，、\s]+/)
    .map(s => s.trim())
    .filter(id => id.length > 0)

  // 去重
  const idList = [...new Set(raw)]

  if (idList.length === 0) {
    message.warning('请至少输入一个漫画ID')
    return
  }

  loading.value = true
  let successCount = 0
  let failCount = 0

  // 串行处理每个ID（避免同时发起太多请求）
  for (const id of idList) {
    try {
      const result = await commands.downloadComic(id)
      if (result.status === 'error') {
        // 如果错误信息包含“所有章节都已存在”等，可以视为正常
        if (result.error.err_message.includes('所有章节都已存在于下载目录')) {
          message.info(`漫画 ${id} 已全部下载完毕，跳过`)
        } else {
          message.error(`下载漫画 ${id} 失败：${result.error.err_message}`)
          failCount++
        }
      } else {
        message.success(`漫画 ${id} 已开始下载所有未下载章节`)
        successCount++
      }
    } catch (e) {
      message.error(`下载漫画 ${id} 时发生异常：${e}`)
      failCount++
    }
  }

  loading.value = false
  message.info(`批量下载完成：成功 ${successCount} 个，失败 ${failCount} 个`)
}
</script>

<template>
  <div class="h-full flex flex-col p-4 gap-4">
    <div class="flex-1 flex flex-col">
      <span class="text-sm text-gray-500">每行输入一个漫画ID，或用逗号/空格分隔</span>
      <n-input
        v-model:value="idsInput"
        type="textarea"
        placeholder="例如：&#10;67a18a90cac76a1659ab71f5&#10;5c8f8f8f8f8f8f8f8f8f8f8f"
        :autosize="{ minRows: 8, maxRows: 20 }"
        class="flex-1"
      />
    </div>
    <n-button
      type="primary"
      size="large"
      :loading="loading"
      :disabled="loading"
      @click="startBatchDownload"
    >
      开始批量下载
    </n-button>
  </div>
</template>