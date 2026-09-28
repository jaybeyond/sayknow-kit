/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"

vi.mock("@/i18n", () => ({ useT: () => ({ t: (key: string) => key }) }))
vi.mock("@/hooks/useProviderProbe", () => ({ useProviderProbe: () => "idle" }))
vi.mock("@/components/OAuthProviderCard", () => ({ OAuthProviderCard: () => null }))

import { ProviderPicker } from "./ProviderPicker"
import { NVIDIA_BASE, OPENROUTER_BASE, ZAI_BASE, ZAI_CODING_BASE } from "@/lib/openrouter"

afterEach(() => cleanup())

function mount(provider: "openrouter" | "zai", baseURL: string) {
  const onChange = vi.fn()
  render(<ProviderPicker provider={provider} baseURL={baseURL} uiLocale="en" onChange={onChange} />)
  return onChange
}

describe("ProviderPicker", () => {
  it("offers NVIDIA and z.ai and switches to their endpoint and default model", () => {
    const onChange = mount("openrouter", OPENROUTER_BASE)
    fireEvent.click(screen.getByText("provider.preset.nvidia"))
    expect(onChange).toHaveBeenLastCalledWith({
      provider: "nvidia",
      baseURL: NVIDIA_BASE,
      model: "deepseek-ai/deepseek-v4.1-flash",
      fallbackModel: "",
    })
    fireEvent.click(screen.getByText("provider.preset.zai"))
    expect(onChange).toHaveBeenLastCalledWith({
      provider: "zai",
      baseURL: ZAI_BASE,
      model: "glm-5.3-flash",
      fallbackModel: "",
    })
  })

  it("does not reset the model when the active provider is clicked again", () => {
    const onChange = mount("zai", ZAI_CODING_BASE)
    fireEvent.click(screen.getByText("provider.preset.zai"))
    expect(onChange).not.toHaveBeenCalled()
  })

  it("switches z.ai between pay-as-you-go and the GLM Coding Plan endpoint", () => {
    const onChange = mount("zai", ZAI_BASE)
    expect(screen.getByRole("radio", { name: "provider.zai.plan.api" }).getAttribute("aria-checked")).toBe("true")
    fireEvent.click(screen.getByRole("radio", { name: "provider.zai.plan.coding" }))
    expect(onChange).toHaveBeenLastCalledWith({ provider: "zai", baseURL: ZAI_CODING_BASE })
  })

  it("shows the plan switch only for z.ai", () => {
    mount("openrouter", OPENROUTER_BASE)
    expect(screen.queryByRole("radiogroup")).toBeNull()
  })
})
