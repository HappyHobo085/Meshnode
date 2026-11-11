<script lang="ts" module>
  // 24×24 line icons, drawn for Meshnode at a 1.75 stroke. Each is a list of SVG path data.
  const PATHS = {
    home: ["M4 10.5 12 4l8 6.5", "M6 9v10h4.5v-5h3v5H18V9"],
    services: ["M4 6.5h16v4H4z", "M4 13.5h16v4H4z", "M7.5 8.5h.01", "M7.5 15.5h.01"],
    logs: ["M5 5h14", "M5 9.5h10", "M5 14h14", "M5 18.5h7"],
    settings: [
      "M12 9.25a2.75 2.75 0 1 0 0 5.5 2.75 2.75 0 0 0 0-5.5Z",
      "M12 3.5v2.2M12 18.3v2.2M3.5 12h2.2M18.3 12h2.2M6 6l1.55 1.55M16.45 16.45 18 18M6 18l1.55-1.55M16.45 7.55 18 6",
    ],
    plus: ["M12 5v14", "M5 12h14"],
    copy: ["M9 9h10v10H9z", "M5 15V5h10"],
    check: ["m5 12.5 4.5 4.5L19 7.5"],
    close: ["m6 6 12 12", "M18 6 6 18"],
    chevron: ["m9.5 6 6 6-6 6"],
    back: ["m14.5 6-6 6 6 6"],
    alert: ["M12 4 3 19.5h18L12 4Z", "M12 10v4", "M12 17h.01"],
    info: ["M12 3.75a8.25 8.25 0 1 0 0 16.5 8.25 8.25 0 0 0 0-16.5Z", "M12 11v5", "M12 8h.01"],
    power: ["M12 4v7", "M7.05 7.05a7 7 0 1 0 9.9 0"],
    folder: ["M3.75 6.75h6l2 2h8.5v9.5H3.75z"],
    file: ["M6.5 3.75h7.5l4 4v12.5H6.5z", "M13.75 3.75v4.25H18"],
    qr: [
      "M4 4h6v6H4z",
      "M14 4h6v6h-6z",
      "M4 14h6v6H4z",
      "M14 14h2.5v2.5H14z",
      "M17.5 17.5H20V20h-2.5z",
    ],
    scan: ["M4 8.5V4h4.5", "M15.5 4H20v4.5", "M20 15.5V20h-4.5", "M8.5 20H4v-4.5", "M7 12h10"],
    desktop: ["M3.75 5h16.5v11H3.75z", "M9 20h6", "M12 16v4"],
    laptop: ["M5.5 6h13v9h-13z", "M3 18.5h18"],
    phone: ["M7.5 3.5h9v17h-9z", "M11 17.5h2"],
    server: ["M5 4.5h14v6H5z", "M5 13.5h14v6H5z", "M8.5 7.5h.01", "M8.5 16.5h.01"],
    trash: ["M4.5 7h15", "M9.5 7V4.5h5V7", "M6.5 7l1 12.5h9l1-12.5"],
    external: ["M13.5 5H19v5.5", "M19 5l-8 8", "M17 13.5V19H5V7h5.5"],
    refresh: ["M19.5 12a7.5 7.5 0 1 1-2.2-5.3", "M19.5 4.5v4h-4"],
    search: ["M10.75 4.5a6.25 6.25 0 1 0 0 12.5 6.25 6.25 0 0 0 0-12.5Z", "m15.25 15.25 4.5 4.5"],
    pause: ["M8.5 5.5v13", "M15.5 5.5v13"],
    play: ["M7.5 5v14l11-7z"],
    link: [
      "M10 14a4 4 0 0 0 5.66 0l3-3a4 4 0 0 0-5.66-5.66l-.75.75",
      "M14 10a4 4 0 0 0-5.66 0l-3 3a4 4 0 0 0 5.66 5.66l.75-.75",
    ],
    more: ["M5.5 12h.01", "M12 12h.01", "M18.5 12h.01"],
    upload: ["M12 15.5V4.5", "m7.5 9 4.5-4.5L16.5 9", "M4.5 15v4.5h15V15"],
    tag: ["M4 4h7.5l8.5 8.5-7.5 7.5L4 11.5z", "M8.25 8.25h.01"],
    mesh: [
      "M6 7.5a2 2 0 1 0 0-.01",
      "M18 7.5a2 2 0 1 0 0-.01",
      "M12 18a2 2 0 1 0 0-.01",
      "M8 7.5h8",
      "m7 9.3 4 6.9",
      "m17 9.3-4 6.9",
    ],
  } as const;

  export type IconName = keyof typeof PATHS;
</script>

<script lang="ts">
  interface Props {
    name: IconName;
    size?: number;
    /** A label makes the icon meaningful to screen readers; without one it is decorative. */
    label?: string;
  }
  let { name, size = 20, label }: Props = $props();
</script>

<svg
  class="icon"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.75"
  stroke-linecap="round"
  stroke-linejoin="round"
  role={label ? "img" : undefined}
  aria-label={label}
  aria-hidden={label ? undefined : "true"}
  focusable="false"
>
  {#each PATHS[name] as d (d)}
    <path {d} />
  {/each}
</svg>

<style>
  .icon {
    flex: none;
  }
</style>
