import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  // Relative base so dist/index.html loads assets standalone (file:// or Tauri) without a dev server
  base: './',
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    // Tauri expects the build output in the `dist` folder
    outDir: 'dist',
    // Tauri needs the build to be in a specific location
    emptyOutDir: true,
  },
  server: {
    // Tauri expects the dev server to run on this port
    port: 1420,
    strictPort: true,
  },
  // Env variables
  envPrefix: ['VITE_', 'TAURI_'],
})