// @vitest-environment jsdom
//
// `wf-image` — the agent showing a picture back. The security property under
// test is that the fence is a REQUEST, not a grant: the id is validated for
// shape, fetched through the host, and a failure states itself rather than
// blanking.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import WfImage from "./WfImage";
import { RichHostProvider } from "./host";
import { MAX_IMAGES, validateImage } from "./spec";

const ID_A = "art_0123456789abcdef0123456789abcdef";
const ID_B = "art_ffffffffffffffffffffffffffffffff";

const fetchMock = vi.fn<(artifactId: string) => Promise<Blob>>();

beforeAll(() => {
  URL.createObjectURL = vi.fn(() => "blob:test");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  cleanup();
  fetchMock.mockReset();
});

const withHost = (node: React.ReactNode) =>
  render(<RichHostProvider host={{ fetchFile: fetchMock }}>{node}</RichHostProvider>);

describe("validateImage", () => {
  it("takes a list and keeps captions and alt text", () => {
    const v = validateImage({
      images: [{ artifact_id: ID_A, caption: "the photo you sent", alt: "a cat" }],
    });
    expect(v.ok).toBe(true);
    if (!v.ok) return;
    expect(v.spec.images[0]).toEqual({
      artifactId: ID_A,
      caption: "the photo you sent",
      alt: "a cat",
    });
  });

  it("refuses a payload that is not a list of images", () => {
    expect(validateImage({ artifact_id: ID_A }).ok).toBe(false);
    expect(validateImage({ images: ID_A }).ok).toBe(false);
    expect(validateImage("nope").ok).toBe(false);
  });

  /// A model that invents an id would otherwise turn a plausible-looking string
  /// into a fetch for something that cannot exist.
  it("drops entries whose id is not a well-formed artifact id", () => {
    const v = validateImage({
      images: [
        { artifact_id: "not-an-id" },
        { artifact_id: "art_XYZ" },
        { artifact_id: ID_A.toUpperCase() },
        { artifact_id: ID_A },
      ],
    });
    expect(v.ok).toBe(true);
    if (!v.ok) return;
    expect(v.spec.images.map((i) => i.artifactId)).toEqual([ID_A]);
  });

  /// All-invalid must fall back to the code block, not render an empty frame
  /// that reads as the block failing.
  it("fails when no entry carries a usable id", () => {
    expect(validateImage({ images: [{ artifact_id: "nope" }, {}] }).ok).toBe(false);
    expect(validateImage({ images: [] }).ok).toBe(false);
  });

  it("caps how many pictures one block may show", () => {
    const many = Array.from({ length: MAX_IMAGES + 5 }, () => ({ artifact_id: ID_A }));
    const v = validateImage({ images: many });
    expect(v.ok).toBe(true);
    if (!v.ok) return;
    expect(v.spec.images.length).toBe(MAX_IMAGES);
  });
});

describe("WfImage", () => {
  it("fetches each picture through the host", async () => {
    fetchMock.mockResolvedValue(new Blob([new Uint8Array([1])], { type: "image/png" }));
    // Queried by tag, not by role: an `alt=""` image is presentational and
    // deliberately absent from the accessibility tree.
    const { container } = withHost(
      <WfImage spec={{ images: [{ artifactId: ID_A }, { artifactId: ID_B }] }} />,
    );
    await waitFor(() => expect(container.querySelectorAll("img")).toHaveLength(2));
    expect(fetchMock).toHaveBeenCalledWith(ID_A);
    expect(fetchMock).toHaveBeenCalledWith(ID_B);
  });

  /// The 404 an owner-mismatched or TTL-swept artifact returns. The reader is
  /// told; nothing about the failure distinguishes "not yours" from "expired",
  /// which is what keeps id probing uninformative.
  it("states that an image is unavailable when the fetch fails", async () => {
    fetchMock.mockRejectedValue(new Error("download failed (HTTP 404)"));
    withHost(<WfImage spec={{ images: [{ artifactId: ID_A, caption: "your selfie" }] }} />);
    await waitFor(() => expect(screen.getByText(/no longer available/i)).toBeTruthy());
    expect(screen.getByText("your selfie")).toBeTruthy();
  });

  /// Without a host that can fetch files there is no authenticated route;
  /// rendering a bare <img> would be the wrong recovery — it would leak the id
  /// into an unauthenticated request.
  it("does not attempt a fetch without a host that fetches files", async () => {
    render(<WfImage spec={{ images: [{ artifactId: ID_A }] }} />);
    await waitFor(() => expect(screen.getByText(/no longer available/i)).toBeTruthy());
    cleanup();
    render(
      <RichHostProvider host={{ submitForm: vi.fn() }}>
        <WfImage spec={{ images: [{ artifactId: ID_A }] }} />
      </RichHostProvider>,
    );
    await waitFor(() => expect(screen.getByText(/no longer available/i)).toBeTruthy());
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("captions the picture and uses alt text for the image itself", async () => {
    fetchMock.mockResolvedValue(new Blob([new Uint8Array([1])], { type: "image/png" }));
    withHost(
      <WfImage spec={{ images: [{ artifactId: ID_A, caption: "Front view", alt: "a red coat" }] }} />,
    );
    await waitFor(() => expect(screen.getByAltText("a red coat")).toBeTruthy());
    expect(screen.getByText("Front view")).toBeTruthy();
  });

  /// The id is not a description. Falling back to it would read the artifact id
  /// aloud to a screen-reader user.
  it("gives an image with nothing to say an empty alt, never the id", async () => {
    fetchMock.mockResolvedValue(new Blob([new Uint8Array([1])], { type: "image/png" }));
    const { container } = withHost(<WfImage spec={{ images: [{ artifactId: ID_A }] }} />);
    await waitFor(() => expect(container.querySelector("img")).toBeTruthy());
    expect(container.querySelector("img")!.getAttribute("alt")).toBe("");
  });

  it("lays several pictures out as a grid and one on its own", async () => {
    fetchMock.mockResolvedValue(new Blob([new Uint8Array([1])], { type: "image/png" }));
    const one = withHost(<WfImage spec={{ images: [{ artifactId: ID_A }] }} />);
    expect(one.container.querySelector(".wf-rimgs-grid")).toBeNull();
    cleanup();
    const two = withHost(<WfImage spec={{ images: [{ artifactId: ID_A }, { artifactId: ID_B }] }} />);
    expect(two.container.querySelector(".wf-rimgs-grid")).toBeTruthy();
  });

  it("loads once the host gains a file fetcher", async () => {
    fetchMock.mockResolvedValue(new Blob([new Uint8Array([1])], { type: "image/png" }));
    const spec = { images: [{ artifactId: ID_A }] };
    const { container, rerender } = render(
      <RichHostProvider host={{}}>
        <WfImage spec={spec} />
      </RichHostProvider>,
    );
    await waitFor(() => expect(screen.getByText(/no longer available/i)).toBeTruthy());
    rerender(
      <RichHostProvider host={{ fetchFile: fetchMock }}>
        <WfImage spec={spec} />
      </RichHostProvider>,
    );
    await waitFor(() => expect(container.querySelector("img")).toBeTruthy());
  });
});
