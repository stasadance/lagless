// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// Served from GitHub Pages at https://stasadance.github.io/lagless/
export default defineConfig({
    site: "https://stasadance.github.io",
    base: "/lagless",
    integrations: [
        starlight({
            title: "lagless",
            description:
                "Low-latency WebRTC data channels for real-time games, in Rust for Node.js. No ping spikes under packet loss.",
            logo: { src: "./src/assets/mark.svg" },
            favicon: "/favicon.svg",
            head: [
                {
                    tag: "link",
                    attrs: {
                        rel: "stylesheet",
                        href: "https://fonts.googleapis.com/css2?family=Archivo:wght@500;800&family=IBM+Plex+Mono:wght@400;600&family=IBM+Plex+Sans:wght@400;600&display=swap",
                    },
                },
            ],
            social: [
                { icon: "github", label: "GitHub", href: "https://github.com/stasadance/lagless" },
            ],
            customCss: ["./src/styles/theme.css"],
            sidebar: [
                { label: "Getting started", items: ["docs", "docs/why", "docs/build"] },
                { label: "Reference", items: ["docs/api", "docs/how-it-works"] },
                { label: "Guides", items: ["docs/netcode"] },
                { label: "Results", items: ["docs/benchmarks"] },
            ],
        }),
    ],
});
