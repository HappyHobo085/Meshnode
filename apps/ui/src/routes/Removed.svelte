<script lang="ts">
  import Icon from "../lib/design/Icon.svelte";
  import { hrefOf } from "../lib/router";
  import ResetIdentity from "./ResetIdentity.svelte";
</script>

<!-- Home for a device whose state is `removed`: what happened, the honest limit, and the
     only way forward (reset, then First run). -->
<section class="removed" aria-labelledby="removed-title">
  <span class="mark" aria-hidden="true"><Icon name="alert" size={28} /></span>
  <h1 id="removed-title">This device was removed from your mesh</h1>
  <p class="lede">
    Another device removed it, so it no longer syncs or runs services. It can never rejoin with the
    same identity.
  </p>
  <p class="lede">
    Reset its identity to start again. You can then create a new mesh, or join yours with a new
    invite from one of your devices.
  </p>
  <div class="actions">
    <ResetIdentity />
    <a class="link" href={hrefOf({ name: "logs" })}>See the log</a>
  </div>
</section>

<style>
  .removed {
    display: grid;
    justify-items: start;
    gap: var(--s-4);
    max-width: 560px;
    padding-top: clamp(var(--s-6), 10vh, var(--s-16));
  }
  .mark {
    display: grid;
    place-items: center;
    width: 56px;
    height: 56px;
    border-radius: var(--r-md);
    background: var(--danger-soft);
    color: var(--danger);
  }
  h1 {
    font-size: var(--fs-3xl);
    letter-spacing: -0.025em;
    max-width: 20ch;
  }
  .lede {
    color: var(--ink-2);
    max-width: 52ch;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-3) var(--s-5);
    margin-top: var(--s-2);
  }
  .link {
    display: inline-flex;
    align-items: center;
    min-height: 34px;
    font-weight: var(--fw-medium);
  }
  @media (pointer: coarse) {
    .link {
      min-height: var(--touch);
    }
  }
  @media (max-width: 639px) {
    h1 {
      font-size: var(--fs-2xl);
    }
  }
</style>
