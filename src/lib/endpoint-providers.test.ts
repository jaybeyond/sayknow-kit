/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"

const fetchMock = vi.hoisted(() => vi.fn())
vi.mock("./http", () => ({ httpFetch: (...args: unknown[]) => fetchMock(...args) }))

import {
  chat,
  endpointKind,
  endpointPreset,
  fetchModels,
  isChatModel,
  NVIDIA_BASE,
  PROVIDER_PRESETS,
  ZAI_BASE,
  ZAI_CODING_BASE,
  zaiReasoningParams,
} from "./openrouter"

function ok(body: unknown) {
  return new Response(JSON.stringify(body), { status: 200 })
}
function fail(status: number) {
  return new Response("nope", { status })
}
const answer = (model: string) => ok({ model, choices: [{ message: { content: " hi " } }] })

function sent(call: number) {
  const [url, init] = fetchMock.mock.calls[call] as [string, RequestInit]
  return { url, body: JSON.parse(String(init.body)), headers: init.headers as Record<string, string> }
}

const messages = [{ role: "user" as const, content: "안녕" }]

afterEach(() => fetchMock.mockReset())

describe("provider presets", () => {
  it("gives NVIDIA and z.ai their own endpoint, key account, and default model", () => {
    expect(PROVIDER_PRESETS.nvidia).toMatchObject({
      baseURL: "https://integrate.api.nvidia.com/v1",
      requiresKey: true,
      keyAccount: "nvidia_api_key",
    })
    expect(PROVIDER_PRESETS.zai).toMatchObject({
      baseURL: "https://api.z.ai/api/paas/v4",
      requiresKey: true,
      keyAccount: "zai_api_key",
    })
    expect(PROVIDER_PRESETS.nvidia.defaultModel).toBeTruthy()
    expect(PROVIDER_PRESETS.zai.defaultModel).toBeTruthy()
    // OpenRouter / OCP / Custom keep sharing the original key.
    expect(PROVIDER_PRESETS.openrouter.keyAccount).toBeUndefined()
    expect(PROVIDER_PRESETS.custom.keyAccount).toBeUndefined()
  })

  it("only recognises endpoint ids as endpoint presets", () => {
    expect(endpointPreset("zai")?.label).toBe("Z.AI")
    expect(endpointPreset("oauth:anthropic")).toBeNull()
    expect(endpointPreset("toString")).toBeNull()
  })
})

describe("endpointKind", () => {
  it("classifies by host, so a Custom entry with a known URL is handled the same", () => {
    expect(endpointKind("https://openrouter.ai/api/v1")).toBe("openrouter")
    expect(endpointKind(NVIDIA_BASE)).toBe("nvidia")
    expect(endpointKind(ZAI_BASE)).toBe("zai")
    expect(endpointKind(ZAI_CODING_BASE)).toBe("zai")
    expect(endpointKind("http://127.0.0.1:3456/v1")).toBe("other")
    expect(endpointKind("not a url")).toBe("other")
  })
})

describe("zaiReasoningParams", () => {
  it("keeps GLM-5.3 thinking shallow and switches it off for the other GLM models", () => {
    expect(zaiReasoningParams("glm-5.3-flash")).toEqual({ reasoning_effort: "low" })
    expect(zaiReasoningParams("GLM-5.3")).toEqual({ reasoning_effort: "low" })
    expect(zaiReasoningParams("glm-5.2")).toEqual({ thinking: { type: "disabled" } })
    expect(zaiReasoningParams("glm-4.5-flash")).toEqual({ thinking: { type: "disabled" } })
    expect(zaiReasoningParams("glm-4-32b-0414-128k")).toEqual({})
  })
})

describe("chat request shape", () => {
  it("sends z.ai a plain model plus the reasoning setting, without OpenRouter headers", async () => {
    fetchMock.mockResolvedValueOnce(answer("glm-5.3-flash"))
    const result = await chat({ apiKey: "zk", baseURL: ZAI_BASE, model: "glm-5.3-flash", messages })
    const { url, body, headers } = sent(0)
    expect(url).toBe("https://api.z.ai/api/paas/v4/chat/completions")
    expect(body).toMatchObject({ model: "glm-5.3-flash", reasoning_effort: "low" })
    expect(body.models).toBeUndefined()
    expect(headers.Authorization).toBe("Bearer zk")
    expect(headers["HTTP-Referer"]).toBeUndefined()
    expect(result.content).toBe("hi")
  })

  it("sends NVIDIA no z.ai-only fields", async () => {
    fetchMock.mockResolvedValueOnce(answer("deepseek-ai/deepseek-v4.1-flash"))
    await chat({ apiKey: "nvapi-x", baseURL: NVIDIA_BASE, model: "deepseek-ai/deepseek-v4.1-flash", messages })
    const { url, body } = sent(0)
    expect(url).toBe("https://integrate.api.nvidia.com/v1/chat/completions")
    expect(body.model).toBe("deepseek-ai/deepseek-v4.1-flash")
    expect(body.thinking).toBeUndefined()
    expect(body.reasoning_effort).toBeUndefined()
  })

  it("keeps OpenRouter's server-side fallback array and attribution headers", async () => {
    fetchMock.mockResolvedValueOnce(answer("a"))
    await chat({ apiKey: "k", baseURL: "https://openrouter.ai/api/v1", model: "a", fallbackModel: "b", messages })
    const { body, headers } = sent(0)
    expect(body.models).toEqual(["a", "b"])
    expect(body.model).toBeUndefined()
    expect(headers["X-Title"]).toBe("SayKnow Kit")
  })

  it("retries the fallback model itself where the endpoint has no `models` array", async () => {
    fetchMock.mockResolvedValueOnce(fail(404)).mockResolvedValueOnce(answer("glm-4.5-flash"))
    const result = await chat({
      apiKey: "zk",
      baseURL: ZAI_BASE,
      model: "glm-5.3",
      fallbackModel: "glm-4.5-flash",
      messages,
    })
    expect(sent(0).body.model).toBe("glm-5.3")
    expect(sent(1).body).toMatchObject({ model: "glm-4.5-flash", thinking: { type: "disabled" } })
    expect(result.model).toBe("glm-4.5-flash")
  })

  it("does not retry a rejected key with the fallback", async () => {
    fetchMock.mockResolvedValueOnce(fail(401))
    await expect(
      chat({ apiKey: "bad", baseURL: NVIDIA_BASE, model: "a", fallbackModel: "b", messages }),
    ).rejects.toThrow(/401/)
    expect(fetchMock).toHaveBeenCalledTimes(1)
  })
})

describe("fetchModels", () => {
  it("drops NVIDIA's embedding, safety and parsing models", async () => {
    fetchMock.mockResolvedValueOnce(
      ok({
        data: [
          { id: "deepseek-ai/deepseek-v4.1-flash" },
          { id: "nvidia/nv-embedqa-mistral-7b-v2" },
          { id: "nvidia/llama-3.1-nemoguard-8b-content-safety" },
          { id: "nvidia/nemotron-parse" },
          { id: "z-ai/glm-5.3-flash" },
        ],
      }),
    )
    const list = await fetchModels("nvapi-x", NVIDIA_BASE)
    expect(list.map((m) => m.id)).toEqual(["deepseek-ai/deepseek-v4.1-flash", "z-ai/glm-5.3-flash"])
  })

  it("leaves other endpoints' lists alone", () => {
    expect(isChatModel("other", "my/embedder")).toBe(true)
  })
})
