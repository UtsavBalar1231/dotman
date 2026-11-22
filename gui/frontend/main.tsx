import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { Workbench } from "./workbench.js";
import "./styles/workbench.css";

const container = document.getElementById("root");
if (container) {
const root = createRoot(container);
flushSync(() => root.render(<Workbench />));
void import("./app.js");
}
