import baseCss from './app.css?inline';
import kurisuCss from './kurisu.css?inline';
import App from './App.svelte';
import { mount } from 'svelte';

// ?theme=kurisu (set in index.html as data-skin) opts into the Kurisu skin;
// everything else gets the default Live Browser look.
const style = document.createElement('style');
style.textContent = document.documentElement.dataset.skin === 'kurisu' ? kurisuCss : baseCss;
document.head.appendChild(style);

mount(App, { target: document.getElementById('app')! });
