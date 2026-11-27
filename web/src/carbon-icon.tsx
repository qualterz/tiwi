import { For } from "solid-js";
import { Dynamic } from "solid-js/web";

export interface CarbonIconDescriptor {
  attrs: Record<string, string | number>;
  content: Array<{ elem: string; attrs: Record<string, string | number> }>;
}

export function CarbonIcon(props: {
  icon: CarbonIconDescriptor;
  class?: string;
}) {
  return (
    <svg
      {...props.icon.attrs}
      class={props.class}
      fill="currentColor"
      aria-hidden="true"
    >
      <For each={props.icon.content}>
        {(el) => <Dynamic component={el.elem} {...el.attrs} />}
      </For>
    </svg>
  );
}
