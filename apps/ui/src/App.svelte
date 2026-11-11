<script lang="ts">
  import { onMount } from "svelte";
  import { api, type Platform } from "./lib/api";
  import Banner from "./lib/design/Banner.svelte";
  import Icon, { type IconName } from "./lib/design/Icon.svelte";
  import Monogram from "./lib/design/Monogram.svelte";
  import Toast from "./lib/design/Toast.svelte";
  import { hrefOf, navigate, route, type Route } from "./lib/router";
  import { banner, members, startPoller, status } from "./lib/stores";
  import { applyTheme } from "./lib/theme";
  import AddDevice from "./routes/AddDevice.svelte";
  import FirstRun from "./routes/FirstRun.svelte";
  import Home from "./routes/Home.svelte";
  import Join from "./routes/Join.svelte";
  import Services from "./routes/Services.svelte";

  interface NavItem {
    label: string;
    icon: IconName;
    to: Route;
    match: Route["name"][];
  }
  const NAV: NavItem[] = [
    { label: "Home", icon: "home", to: { name: "home" }, match: ["home", "add", "join"] },
    {
      label: "Services",
      icon: "services",
      to: { name: "services" },
      match: ["services", "service", "deploy"],
    },
    { label: "Logs", icon: "logs", to: { name: "logs" }, match: ["logs"] },
    { label: "Settings", icon: "settings", to: { name: "settings" }, match: ["settings"] },
  ];

  /** Screens that take the whole window, without navigation. */
  const BARE: Route["name"][] = ["first-run", "no-daemon", "stopped"];

  let platform: Platform | null = $state(null);
  const self = $derived($members.find((m) => m.isSelf) ?? null);
  const uninitialised = $derived($status?.state === "uninitialised");
  // Before this device belongs to a mesh, Join is part of the full-window first-run flow.
  const bare = $derived(BARE.includes($route.name) || (uninitialised && $route.name === "join"));

  // A device without a mesh can only create or join one; once it has one, first run is over.
  $effect(() => {
    const name = $route.name;
    if (uninitialised && name !== "first-run" && name !== "join") {
      navigate({ name: "first-run" }, { replace: true });
    } else if ($status?.state === "running" && name === "first-run") {
      navigate({ name: "home" }, { replace: true });
    }
  });

  onMount(() => {
    const stopTheme = applyTheme();
    const stopPoller = startPoller();
    api
      .platform()
      .then((p) => {
        platform = p;
        document.documentElement.dataset.platform = p;
      })
      .catch(() => {
        platform = null;
      });
    return () => {
      stopPoller();
      stopTheme();
    };
  });
</script>

<div class={["shell", { bare }]}>
  {#if !bare}
    <nav class="sidebar" aria-label="Main">
      <a class="brand" href={hrefOf({ name: "home" })}>
        <span class="mark"><Icon name="mesh" size={22} /></span>
        <span class="wordmark">Meshnode</span>
      </a>
      <ul>
        {#each NAV as item (item.label)}
          {@const current = item.match.includes($route.name)}
          <li>
            <a href={hrefOf(item.to)} aria-current={current ? "page" : undefined}>
              <Icon name={item.icon} />
              <span class="label">{item.label}</span>
            </a>
          </li>
        {/each}
      </ul>
      {#if self}
        <div class="self">
          <Monogram name={self.name} hue={self.accentHue} size="sm" online={self.online} />
          <div class="self-text">
            <span class="self-name">{self.name}</span>
            <span class="self-sub num">
              {$status ? `${$status.membersOnline} of ${$status.membersTotal} online` : ""}
            </span>
          </div>
        </div>
      {/if}
    </nav>
  {/if}

  <div class="main">
    <Banner state={$banner} />
    <main id="content" tabindex="-1">
      {#if $route.name === "home"}
        <Home />
      {:else if $route.name === "add"}
        <AddDevice />
      {:else if $route.name === "join"}
        <Join />
      {:else if $route.name === "first-run"}
        <FirstRun />
      {:else if $route.name === "services" || $route.name === "service" || $route.name === "deploy"}
        <!-- One Services screen: the detail and deploy sheets open over its list. -->
        <Services
          detail={$route.name === "service" ? $route.params.name : null}
          deploying={$route.name === "deploy"}
        />
      {:else}
        <!-- The remaining screens arrive in the next tasks. -->
        <section class="placeholder" data-route={$route.name} data-platform={platform}>
          <h1>{NAV.find((n) => n.match.includes($route.name))?.label ?? $route.name}</h1>
        </section>
      {/if}
    </main>
  </div>

  {#if !bare}
    <nav class="tabs" aria-label="Main">
      {#each NAV as item (item.label)}
        {@const current = item.match.includes($route.name)}
        <a href={hrefOf(item.to)} aria-current={current ? "page" : undefined}>
          <Icon name={item.icon} size={22} />
          <span>{item.label}</span>
        </a>
      {/each}
    </nav>
  {/if}
</div>

<Toast />

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-w) minmax(0, 1fr);
    min-height: 100dvh;
  }
  .shell.bare {
    grid-template-columns: minmax(0, 1fr);
  }

  /* Sidebar: quiet, flush with the background, separated by a hairline. */
  .sidebar {
    position: sticky;
    top: 0;
    display: flex;
    flex-direction: column;
    gap: var(--s-6);
    height: 100dvh;
    padding: var(--s-5) var(--s-3);
    border-right: 1px solid var(--line);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    padding: 0 var(--s-3);
    color: var(--ink);
    text-decoration: none;
  }
  .mark {
    display: grid;
    place-items: center;
    width: 34px;
    height: 34px;
    border-radius: var(--r-sm);
    background: var(--signal-soft);
    color: var(--signal);
  }
  .wordmark {
    font-size: var(--fs-lg);
    font-weight: var(--fw-semibold);
    letter-spacing: -0.02em;
  }
  ul {
    display: grid;
    gap: 2px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .sidebar a:not(.brand) {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--s-3);
    min-height: 40px;
    padding: 0 var(--s-3);
    border-radius: var(--r-sm);
    color: var(--ink-2);
    font-weight: var(--fw-medium);
    text-decoration: none;
    transition:
      background-color var(--dur-1) var(--ease-out),
      color var(--dur-1) var(--ease-out);
  }
  .sidebar a:not(.brand):hover {
    background: var(--surface-2);
    color: var(--ink);
  }
  .sidebar a[aria-current="page"] {
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--e-1);
  }
  .sidebar a[aria-current="page"] :global(.icon) {
    color: var(--signal);
  }
  .self {
    display: flex;
    align-items: center;
    gap: var(--s-3);
    margin-top: auto;
    padding: var(--s-3);
    border-top: 1px solid var(--line);
  }
  .self-text {
    display: grid;
    min-width: 0;
  }
  .self-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: var(--fs-sm);
    font-weight: var(--fw-semibold);
  }
  .self-sub {
    font-size: var(--fs-xs);
    color: var(--ink-3);
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  main {
    width: 100%;
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--s-8) var(--s-10) var(--s-12);
  }
  main:focus-visible {
    box-shadow: none;
  }

  .tabs {
    display: none;
  }

  /* Narrow desktop windows: the sidebar folds to an icon rail. */
  @media (max-width: 799px) {
    .shell {
      grid-template-columns: var(--rail-w) minmax(0, 1fr);
    }
    .sidebar {
      align-items: center;
      padding: var(--s-4) var(--s-2);
    }
    .wordmark,
    .sidebar .label,
    .self-text {
      display: none;
    }
    .brand,
    .sidebar a:not(.brand) {
      justify-content: center;
      width: 48px;
      padding: 0;
    }
    .sidebar a:not(.brand) {
      min-height: 48px;
    }
    .self {
      padding: var(--s-3) 0 0;
    }
    main {
      padding: var(--s-6);
    }
  }

  /* Phones: bottom tabs, respecting the safe areas. */
  @media (max-width: 639px) {
    .shell,
    .shell.bare {
      grid-template-columns: minmax(0, 1fr);
    }
    .sidebar {
      display: none;
    }
    .main {
      padding-top: var(--safe-top);
    }
    main {
      padding: var(--s-5) calc(var(--s-4) + var(--safe-right))
        calc(var(--tabbar-h) + var(--s-8) + var(--safe-bottom)) calc(var(--s-4) + var(--safe-left));
    }
    .tabs {
      position: fixed;
      inset: auto 0 0;
      z-index: 40;
      display: grid;
      grid-template-columns: repeat(4, 1fr);
      padding: 0 var(--safe-right) var(--safe-bottom) var(--safe-left);
      background: color-mix(in oklab, var(--surface) 92%, transparent);
      backdrop-filter: blur(12px);
      border-top: 1px solid var(--line);
    }
    .tabs a {
      display: grid;
      justify-items: center;
      align-content: center;
      gap: 2px;
      height: var(--tabbar-h);
      color: var(--ink-3);
      font-size: var(--fs-xs);
      font-weight: var(--fw-medium);
      text-decoration: none;
    }
    .tabs a[aria-current="page"] {
      color: var(--signal);
    }
    .tabs a:focus-visible {
      border-radius: var(--r-sm);
    }
  }
</style>
