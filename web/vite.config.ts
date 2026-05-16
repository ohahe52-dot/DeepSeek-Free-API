import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'
import fs from 'fs'

const configPath = path.resolve(__dirname, '../config.toml')
let backendPort = 22217
try {
  const configToml = fs.readFileSync(configPath, 'utf-8')
  const portMatch = configToml.match(/^port\s*=\s*(\d+)/m)
  if (portMatch) backendPort = parseInt(portMatch[1], 10)
} catch {
  // Dùng port mặc định khi config.toml không tồn tại
}

export default defineConfig({
  base: '/admin/',
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    proxy: {
      '/admin/api': `http://127.0.0.1:${backendPort}`,
      '/v1': `http://127.0.0.1:${backendPort}`,
      '/anthropic': `http://127.0.0.1:${backendPort}`,
    },
  },
})
