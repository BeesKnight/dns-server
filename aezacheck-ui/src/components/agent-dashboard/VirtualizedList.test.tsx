import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { VirtualizedList } from "./VirtualizedList";

describe("VirtualizedList", () => {
  it("renders only visible items with overscan", async () => {
    const items = Array.from({ length: 100 }, (_, index) => `Item ${index}`);

    const { container } = render(
      <VirtualizedList
        items={items}
        itemHeight={40}
        overscan={1}
        className="test-virtual-list"
        renderItem={(item) => (
          <div data-testid="virtual-item" className="p-2 text-xs">
            {item}
          </div>
        )}
      />
    );

    expect(screen.getAllByTestId("virtual-item").length).toBeLessThanOrEqual(4);

    const list = container.querySelector(".test-virtual-list") as HTMLDivElement;
    Object.defineProperty(list, "scrollTop", { value: 400, writable: true });
    fireEvent.scroll(list);

    await waitFor(() => {
      expect(screen.getByText("Item 10")).toBeInTheDocument();
    });
  });
});
