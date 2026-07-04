<script setup lang="ts">
// Thin wrapper around the asciinema-player CDN bundle (loaded via a <script
// defer> tag injected by config.mts's transformHead, home page only). The
// bundle sets a `window.AsciinemaPlayer` global; since it's `defer`red we
// can't assume it's ready by the time this component mounts, so we poll for
// it briefly instead of re-injecting or bundling the library ourselves.
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { withBase } from 'vitepress'

const props = withDefaults(
  defineProps<{
    src: string
    autoPlay?: boolean
    rows?: number
    cols?: number
  }>(),
  {
    autoPlay: false,
    rows: 32,
    cols: 120,
  },
)

const el = ref<HTMLDivElement | null>(null)
let player: { dispose(): void } | undefined
let poll: ReturnType<typeof setInterval> | undefined

function waitForAsciinemaPlayer(): Promise<any> {
  const existing = (window as any).AsciinemaPlayer
  if (existing) return Promise.resolve(existing)
  return new Promise((resolve, reject) => {
    let waited = 0
    poll = setInterval(() => {
      const found = (window as any).AsciinemaPlayer
      if (found) {
        clearInterval(poll)
        resolve(found)
        return
      }
      waited += 100
      if (waited > 10_000) {
        clearInterval(poll)
        reject(new Error('asciinema-player did not load in time'))
      }
    }, 100)
  })
}

onMounted(async () => {
  let AsciinemaPlayer: any
  try {
    AsciinemaPlayer = await waitForAsciinemaPlayer()
  } catch {
    return
  }
  if (!el.value) return
  player = AsciinemaPlayer.create(withBase(props.src), el.value, {
    autoPlay: props.autoPlay,
    loop: true,
    speed: 1.0,
    idleTimeLimit: 1.0,
    theme: 'asciinema',
    cols: props.cols,
    rows: props.rows,
    fit: 'width',
    poster: 'npt:0:00',
  })
})

onBeforeUnmount(() => {
  if (poll) clearInterval(poll)
  player?.dispose()
})
</script>

<template>
  <div ref="el" class="asciinema-embed" />
</template>

<style scoped>
.asciinema-embed {
  margin-top: 16px;
}
</style>
