import "./lib/design/tokens.css";
import "./lib/design/base.css";
import { mount } from "svelte";
import App from "./App.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("#app is missing from index.html");

export default mount(App, { target });
