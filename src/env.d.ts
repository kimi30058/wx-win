/// <reference types="vite/client" />

// .vue 单文件组件的 TS 模块声明（strict 模式下 vue-tsc 需要）
declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<object, object, unknown>;
  export default component;
}
