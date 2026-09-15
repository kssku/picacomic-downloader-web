<script setup lang="ts">
import { onMounted } from 'vue'
import { commands, events } from '../../bindings.ts'
import { useStore } from '../../store.ts'
import UncompletedProgresses from './components/UncompletedProgresses.vue'
import CompletedProgresses from './components/CompletedProgresses.vue'
import { ProgressData } from '../../types.ts'

export type ProgressesPaneTabName = 'uncompleted' | 'completed'

const store = useStore()

onMounted(async () => {
  await events.downloadTaskEvent.listen(async ({ payload: { event, data } }) => {
    if (event === 'Create') {
      const { chapterInfo, downloadedImgCount, totalImgCount } = data

      store.progresses.set(chapterInfo.chapterId, {
        ...data,
        percentage: 0,
        indicator: `排队中 ${downloadedImgCount}/${totalImgCount}`,
      })
    } else if (event === 'Update') {
      const { chapterId, state, downloadedImgCount, totalImgCount } = data

      const progressData = store.progresses.get(chapterId)
      if (progressData === undefined) {
        return
      }

      progressData.state = state
      progressData.downloadedImgCount = downloadedImgCount
      progressData.totalImgCount = totalImgCount

      if (state === 'Completed') {
        progressData.chapterInfo.isDownloaded = true
        await syncPickedComic()
        await syncComicInSearch(progressData)
        await syncComicInFavorite(progressData)
      }

      progressData.percentage = (downloadedImgCount / totalImgCount) * 100

      let indicator = ''
      if (state === 'Pending') {
        indicator = `排队中`
      } else if (state === 'Downloading') {
        indicator = `下载中`
      } else if (state === 'Paused') {
        indicator = `已暂停`
      } else if (state === 'Cancelled') {
        indicator = `已取消`
      } else if (state === 'Completed') {
        indicator = `下载完成`
      } else if (state === 'Failed') {
        indicator = `下载失败`
      }
      if (totalImgCount !== 0) {
        indicator += ` ${downloadedImgCount}/${totalImgCount}`
      }

      progressData.indicator = indicator
    }
  })
})

async function syncPickedComic() {
  if (store.pickedComic === undefined) {
    return
  }
  const result = await commands.getSyncedComic(store.pickedComic)
  if (result.status === 'error') {
    console.error(result.error)
    return
  }
  store.pickedComic = result.data
}

async function syncComicInSearch(progressData: ProgressData) {
  if (store.searchResult === undefined) {
    return
  }
  const comic = store.searchResult.docs.find((comic) => comic.id === progressData.comic.id)
  if (comic === undefined) {
    return
  }
  const result = await commands.getSyncedComicInSearch(comic)
  if (result.status === 'error') {
    console.error(result.error)
    return
  }
  Object.assign(comic, { ...result.data })
}

async function syncComicInFavorite(progressData: ProgressData) {
  if (store.getFavoriteResult === undefined) {
    return
  }
  const comic = store.getFavoriteResult.docs.find((comic) => comic.id === progressData.comic.id)
  if (comic === undefined) {
    return
  }
  const result = await commands.getSyncedComicInFavorite(comic)
  if (result.status === 'error') {
    console.error(result.error)
    return
  }
  Object.assign(comic, { ...result.data })
}
</script>

<template>
  <div class="flex flex-col gap-2 flex-1 overflow-auto">
    <n-tabs class="h-full overflow-auto" v-model:value="store.progressesPaneTabName" type="line" size="small">
      <n-tab-pane class="h-full p-0! overflow-auto" name="uncompleted" tab="未完成">
        <uncompleted-progresses />
      </n-tab-pane>
      <n-tab-pane class="h-full p-0! overflow-auto" name="completed" tab="已完成">
        <completed-progresses />
      </n-tab-pane>
    </n-tabs>
  </div>
</template>

<style scoped>
:deep(.n-tabs-tab) {
  @apply important-py-0.75;
}
</style>