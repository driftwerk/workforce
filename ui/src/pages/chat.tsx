import { useEffect, useRef, useCallback } from "react";
import { Bot, Loader2, Wifi, WifiOff, Shrink } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { EmptyState } from "@/components/empty-state";
import { MessageBubble } from "@/components/chat/message-bubble";
import { ChatInput } from "@/components/chat/chat-input";
import { useAgents } from "@/hooks/use-agents";
import { useChatStore } from "@/stores/chat-store";

export default function ChatPage() {
  const { data: agentData, isLoading: agentsLoading } = useAgents();
  const agents = agentData?.agents ?? [];

  const {
    connected,
    connecting,
    agentId,
    messages,
    welcomeText,
    isStreaming,
    isTyping,
    isCompacting,
    isUploading,
    contextTokens,
    contextMax,
    connect,
    disconnect,
    send,
    cancel,
    clearMessages,
  } = useChatStore();

  const messagesEndRef = useRef<HTMLDivElement>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const isNearBottom = useRef(true);

  // Default to first agent when loaded
  useEffect(() => {
    if (!agentId && agents.length > 0) {
      const defaultAgent = agents.find((a) => a.default) ?? agents[0];
      if (defaultAgent) connect(defaultAgent.id);
    }
  }, [agents, agentId, connect]);

  // Cleanup on unmount
  useEffect(() => {
    return () => disconnect();
  }, [disconnect]);

  // Smart auto-scroll: only scroll down if user is near the bottom
  useEffect(() => {
    if (isNearBottom.current) {
      messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [messages, isTyping]);

  const handleScroll = useCallback(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const threshold = 100;
    isNearBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < threshold;
  }, []);

  const handleAgentChange = (newAgentId: string) => {
    if (newAgentId === agentId) return;
    disconnect();
    clearMessages();
    connect(newAgentId);
  };

  const selectedAgent = agents.find((a) => a.id === agentId);
  const agentName = selectedAgent?.name ?? agentId ?? "Agent";

  return (
    <div className="flex flex-col h-[calc(100vh-4rem)]">
      {/* Header */}
      <div className="p-4 border-b flex items-center justify-between gap-3 flex-wrap shrink-0">
        <div className="flex items-center gap-3">
          <div>
            <h1 className="text-lg font-bold">Chat</h1>
            <p className="text-xs text-muted-foreground">Send messages to your agents.</p>
          </div>
          {/* Connection status */}
          <Badge variant={connected ? "secondary" : "outline"} className="text-[10px] gap-1">
            {connecting ? (
              <Loader2 className="h-2.5 w-2.5 animate-spin" />
            ) : connected ? (
              <Wifi className="h-2.5 w-2.5" />
            ) : (
              <WifiOff className="h-2.5 w-2.5" />
            )}
            {connecting ? "Connecting" : connected ? "Connected" : "Disconnected"}
          </Badge>
        </div>
        <select
          value={agentId ?? ""}
          onChange={(e) => handleAgentChange(e.target.value)}
          disabled={agentsLoading}
          className="h-9 rounded-md border border-input bg-background px-3 py-1 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {agentsLoading && <option>Loading agents...</option>}
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name ?? a.id}
            </option>
          ))}
        </select>
      </div>

      {/* Messages area */}
      <div
        ref={scrollContainerRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto"
      >
        {messages.length === 0 ? (
          <div className="flex items-center justify-center h-full">
            <div className="text-center space-y-3">
              <EmptyState
                icon={Bot}
                title={`Chat with ${agentName}`}
                description={welcomeText ?? (agentId ? "Send a message to begin." : "Select an agent to begin.")}
              />
            </div>
          </div>
        ) : (
          <div className="max-w-3xl mx-auto px-4 py-4 divide-y divide-border/50">
            {messages.map((msg) => (
              <MessageBubble
                key={msg.id}
                role={msg.role}
                content={msg.content}
                timestamp={msg.timestamp}
                status={msg.status}
                toolCalls={msg.toolCalls}
                agentId={agentId ?? undefined}
                codeCitations={msg.codeCitations}
                routeLine={msg.routeLine}
                connectPrompt={msg.connectPrompt}
                error={msg.error}
                attachments={msg.attachments}
                sessionId={msg.sessionId}
              />
            ))}

            {/* Typing indicator */}
            {isTyping && !isStreaming && (
              <div className="flex items-center gap-3 py-3">
                <div className="flex items-center justify-center w-7 h-7 rounded-full bg-violet-600 text-white shrink-0">
                  <Bot className="h-3.5 w-3.5" />
                </div>
                <div className="flex items-center gap-1.5">
                  <span className="w-1.5 h-1.5 rounded-full bg-muted-foreground animate-bounce" style={{ animationDelay: "0ms" }} />
                  <span className="w-1.5 h-1.5 rounded-full bg-muted-foreground animate-bounce" style={{ animationDelay: "150ms" }} />
                  <span className="w-1.5 h-1.5 rounded-full bg-muted-foreground animate-bounce" style={{ animationDelay: "300ms" }} />
                </div>
              </div>
            )}

            {/* Compacting indicator */}
            {isCompacting && (
              <div className="flex items-center gap-2 py-2 text-xs text-muted-foreground">
                <Shrink className="h-3.5 w-3.5 animate-pulse" />
                <span>Compacting context...</span>
              </div>
            )}

            <div ref={messagesEndRef} />
          </div>
        )}
      </div>

      {/* Input */}
      <div className="p-4 border-t shrink-0 max-w-3xl mx-auto w-full">
        <Card>
          <CardContent className="p-3">
            <ChatInput
              onSend={send}
              onCancel={cancel}
              disabled={!connected || !agentId || isUploading}
              isStreaming={isStreaming}
              contextTokens={contextTokens}
              contextMax={contextMax}
              placeholder={
                !connected
                  ? "Connecting..."
                  : isUploading
                    ? "Uploading attachment…"
                    : agentId
                      ? `Message ${agentName}... (Enter to send)`
                      : "Select an agent first"
              }
            />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
