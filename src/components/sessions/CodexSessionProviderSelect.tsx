import { useTranslation } from "react-i18next";
import type { Provider } from "@/types";
import { ProviderHealthBadge } from "@/components/providers/ProviderHealthBadge";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import { useProviderHealth } from "@/lib/query/failover";

const GLOBAL_ROUTE_VALUE = "__follow_global_route__";

interface CodexProviderOptionProps {
  provider: Provider;
}

function CodexProviderOption({ provider }: CodexProviderOptionProps) {
  const { t } = useTranslation();
  const { data: health } = useProviderHealth(provider.id, "codex");

  return (
    <div className="flex min-w-[15rem] items-center justify-between gap-3">
      <span className="min-w-0 flex-1 truncate">{provider.name}</span>
      {health ? (
        <ProviderHealthBadge
          consecutiveFailures={health.consecutive_failures}
          isHealthy={health.is_healthy}
          className="shrink-0 px-1.5 py-0.5"
        />
      ) : (
        <Badge
          variant="outline"
          className="shrink-0 px-1.5 py-0.5 font-normal text-muted-foreground"
        >
          {t("sessionManager.providerHealthUnknown", {
            defaultValue: "状态未知",
          })}
        </Badge>
      )}
    </div>
  );
}

interface CodexSessionProviderSelectProps {
  pinnedProviderId?: string | null;
  providers: Provider[];
  disabled?: boolean;
  onChange: (providerId: string | null) => void;
}

export function CodexSessionProviderSelect({
  pinnedProviderId,
  providers,
  disabled = false,
  onChange,
}: CodexSessionProviderSelectProps) {
  const { t } = useTranslation();
  const selectedProvider = pinnedProviderId
    ? providers.find((provider) => provider.id === pinnedProviderId)
    : undefined;
  const selectedLabel = pinnedProviderId
    ? (selectedProvider?.name ??
      t("sessionManager.unknownPinnedProvider", {
        defaultValue: "未知供应商",
      }))
    : t("sessionManager.followGlobalRoute", {
        defaultValue: "跟随全局路由",
      });

  return (
    <Select
      value={pinnedProviderId ?? GLOBAL_ROUTE_VALUE}
      disabled={disabled}
      onValueChange={(routeValue) =>
        onChange(routeValue === GLOBAL_ROUTE_VALUE ? null : routeValue)
      }
    >
      <SelectTrigger
        className="h-8 w-auto max-w-[18rem] gap-1.5 px-2.5"
        aria-label={t("sessionManager.sessionProviderAriaLabel", {
          defaultValue: "供应商：{{provider}}",
          provider: selectedLabel,
        })}
      >
        <span className="shrink-0 text-xs text-muted-foreground">
          {t("sessionManager.sessionProvider", {
            defaultValue: "供应商",
          })}
          ：
        </span>
        <span className="min-w-0 truncate text-xs font-medium">
          {selectedLabel}
        </span>
      </SelectTrigger>
      <SelectContent className="min-w-[18rem]">
        <SelectItem value={GLOBAL_ROUTE_VALUE}>
          <div className="flex min-w-[15rem] items-center justify-between gap-3">
            <span>
              {t("sessionManager.followGlobalRoute", {
                defaultValue: "跟随全局路由",
              })}
            </span>
            <Badge
              variant="outline"
              className="shrink-0 px-1.5 py-0.5 font-normal text-muted-foreground"
            >
              {t("sessionManager.globalRouteBadge", {
                defaultValue: "全局",
              })}
            </Badge>
          </div>
        </SelectItem>
        {providers.map((provider) => (
          <SelectItem key={provider.id} value={provider.id}>
            <CodexProviderOption provider={provider} />
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
