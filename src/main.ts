import { createApp } from 'vue';
import { createPinia } from 'pinia';
import TDesign from 'tdesign-vue-next';
import 'tdesign-vue-next/es/style/index.css';

import App from './App.vue';

// 桌面面板入口：pinia + TDesign 全量注册（单窗口小体量，不按需拆）
const app = createApp(App);
app.use(createPinia());
app.use(TDesign);
app.mount('#app');
