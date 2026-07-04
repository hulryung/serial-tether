import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import AsciinemaPlayer from './AsciinemaPlayer.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('AsciinemaPlayer', AsciinemaPlayer)
  },
} satisfies Theme
