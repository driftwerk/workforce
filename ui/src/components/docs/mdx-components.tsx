import defaultMdxComponents from "fumadocs-ui/mdx";
import { Accordion, Accordions } from "fumadocs-ui/components/accordion";
import { Step, Steps } from "fumadocs-ui/components/steps";
import { Tab, Tabs } from "fumadocs-ui/components/tabs";
import type { MDXComponents } from "mdx/types";

const docsComponents: MDXComponents = {
  Accordion,
  Accordions,
  Step,
  Steps,
  Tab,
  Tabs,
};

export function useMDXComponents(overrides?: MDXComponents): MDXComponents {
  return { ...defaultMdxComponents, ...docsComponents, ...overrides };
}
