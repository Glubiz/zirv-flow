import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, RouterProvider } from "react-router";

import { AppRoutes } from "./app/App";
import "./ui/styles.css";

const router = createBrowserRouter([{ path: "*", element: <AppRoutes /> }]);

const root = document.getElementById("root");
if (!root) {
  throw new Error("The page has no #root element");
}

createRoot(root).render(
  <StrictMode>
    <RouterProvider router={router} />
  </StrictMode>,
);
