import { defineStore } from 'pinia'
import { CurrentTabName, ProgressData } from './types.ts'
import { Comic, Config, SearchResult, ServerInfo, UserProfileDetailRespData } from './bindings'
import { ref } from 'vue'
import { ProgressesPaneTabName } from './panes/ProgressesPane/ProgressesPane.vue'

export const useStore = defineStore('store', () => {
  const config = ref<Config>()
  const serverInfo = ref<ServerInfo>()
  const userProfile = ref<UserProfileDetailRespData>()
  const pickedComic = ref<Comic>()
  const currentTabName = ref<CurrentTabName>('search')
  const progresses = ref<Map<string, ProgressData>>(new Map())
  const searchResult = ref<SearchResult>()
  const progressesPaneTabName = ref<ProgressesPaneTabName>('uncompleted')

  return {
    config,
    serverInfo,
    userProfile,
    pickedComic,
    currentTabName,
    progresses,
    searchResult,
    progressesPaneTabName,
  }
})
