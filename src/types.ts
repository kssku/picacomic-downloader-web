import { DownloadTaskEvent } from './bindings'

export type CurrentTabName = 'search' | 'favorite' | 'downloaded' | 'chapter' | 'batch'

export type ProgressData = Extract<DownloadTaskEvent, { event: 'Create' }>['data'] & {
  percentage: number
  indicator: string
}