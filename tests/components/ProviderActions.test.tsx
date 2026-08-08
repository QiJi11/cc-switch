import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProviderActions } from "@/components/providers/ProviderActions";

describe("ProviderActions", () => {
  it("keeps manual switching separate from failover queue management", () => {
    const switchProvider = vi.fn();
    const toggleFailover = vi.fn();

    render(
      <ProviderActions
        appId="codex"
        isCurrent={false}
        isProxyTakeover
        isAutoFailoverEnabled
        isInFailoverQueue={false}
        onSwitch={switchProvider}
        onToggleFailover={toggleFailover}
        onEdit={vi.fn()}
        onDuplicate={vi.fn()}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "provider.enable" }));
    expect(switchProvider).toHaveBeenCalledTimes(1);
    expect(toggleFailover).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "加入队列" }));
    expect(toggleFailover).toHaveBeenCalledWith(true);
  });
});
