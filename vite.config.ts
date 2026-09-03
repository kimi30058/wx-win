import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// 桌面 App 前端：构建产物 ../dist 供 Tauri 壳加载（tauri.conf.json frontendDist）
// clearScreen 保持 CI 日志可读；server.port 固定 5173 与 tauri.conf.json devUrl 对齐
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: 'dist',
    target: 'es2021',
  },
});
