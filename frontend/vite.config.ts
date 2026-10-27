import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// paper-bro is served under /paper-bro on aero.oops.wtf, so every asset URL
// carries the prefix. In dev, the API is the local backend on :8083.
export default defineConfig({
  base: '/paper-bro/',
  plugins: [react()],
  server: {
    proxy: { '/paper-bro/api': 'http://127.0.0.1:8083' },
  },
})
