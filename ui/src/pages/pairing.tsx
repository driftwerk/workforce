import { Network } from "lucide-react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PendingRequests } from "@/components/pairing/pending-requests";
import { DeviceList } from "@/components/pairing/device-list";

export default function PairingPage() {
  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-3">
        <div className="rounded-full bg-muted p-2">
          <Network className="h-5 w-5 text-muted-foreground" />
        </div>
        <div>
          <h1 className="text-lg font-semibold">Device Pairing</h1>
          <p className="text-sm text-muted-foreground">
            Manage device pairing requests and approved devices.
          </p>
        </div>
      </div>

      <Tabs defaultValue="pending">
        <TabsList>
          <TabsTrigger value="pending">Pending</TabsTrigger>
          <TabsTrigger value="devices">Devices</TabsTrigger>
        </TabsList>
        <TabsContent value="pending">
          <PendingRequests />
        </TabsContent>
        <TabsContent value="devices">
          <DeviceList />
        </TabsContent>
      </Tabs>
    </div>
  );
}
