import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("access_token", "e2e-token");
  });

  await page.route("**/api/v1/auth/me", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "user-1", email: "qa@example.com", role: "admin" }),
    });
  });

  await page.route("**/api/agents?**", async (route) => {
    const url = new URL(route.request().url());
    const pageParam = Number(url.searchParams.get("page") ?? "1");
    const response = {
      items: [
        {
          id: "agent-1",
          name: "QA Agent",
          role: "monitor",
          status: "idle",
          lastActiveAt: new Date().toISOString(),
          tasksInFlight: 2,
          capabilities: ["http"],
          metadata: {},
        },
      ],
      page: pageParam,
      pageSize: Number(url.searchParams.get("pageSize") ?? "100"),
      total: 1,
    };
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(response),
    });
  });

  await page.route("**/api/agents/agent-1/tasks**", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        items: [
          {
            id: "task-1",
            agentId: "agent-1",
            title: "Scan",
            status: "running",
            progress: 55,
            queuedAt: new Date().toISOString(),
          },
        ],
        page: 1,
        pageSize: 50,
        total: 1,
      }),
    });
  });

  await page.route("**/api/tasks**", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        items: [
          {
            id: "task-1",
            agentId: "agent-1",
            title: "Scan",
            status: "running",
            progress: 55,
            queuedAt: new Date().toISOString(),
          },
        ],
        page: 1,
        pageSize: 50,
        total: 1,
      }),
    });
  });

  await page.route("**/api/interactions**", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        items: [
          {
            id: "interaction-1",
            agentId: "agent-1",
            taskId: "task-1",
            direction: "response",
            message: "Завершено",
            createdAt: new Date().toISOString(),
          },
        ],
        page: 1,
        pageSize: 50,
        total: 1,
      }),
    });
  });
});

test("отображает список агентов и детали", async ({ page }) => {
  await page.goto("/app/agents");
  await expect(page.getByText("Консоль агентов")).toBeVisible();
  await expect(page.getByText("QA Agent")).toBeVisible();
  await expect(page.getByText("Статусы задач")).toBeVisible();
  await expect(page.getByText("История взаимодействий")).toBeVisible();
});
