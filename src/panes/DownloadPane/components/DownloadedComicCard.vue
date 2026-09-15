<script setup lang="ts">
import { Comic } from '../../../bindings.ts'
import { useStore } from '../../../store.ts'

const props = defineProps<{
  comic: Comic
  checkboxChecked: (comic: Comic) => boolean
  handleCheckboxClick: (comic: Comic) => void
  handleContextMenu: (comic: Comic) => void
}>()

const store = useStore()

function pickComic() {
  store.pickedComic = props.comic
  store.currentTabName = 'chapter'
}
</script>

<template>
  <div class="flex relative border border-solid rounded-md border-gray-2 p-1" @contextmenu="handleContextMenu(comic)">
    <n-checkbox
      size="large"
      class="absolute top-3 left-3 z-1"
      :checked="checkboxChecked(comic)"
      @click="handleCheckboxClick(comic)" />
    <img
      class="w-24 object-cover mr-4"
      :src="`${comic.thumb.fileServer}/static/${comic.thumb.path}`"
      alt=""
      :draggable="false"
      referrerpolicy="no-referrer" />
    <div class="flex flex-col w-full">
      <span
        class="font-bold text-lg line-clamp-2 cursor-pointer transition-colors duration-200 hover:text-blue-5"
        @click="pickComic">
        {{ comic.title }}
      </span>
      <span class="text-red">作者：{{ comic.author }}</span>
      <span class="text-gray" v-html="`分类：${comic.categories}`"></span>
    </div>
  </div>
</template>