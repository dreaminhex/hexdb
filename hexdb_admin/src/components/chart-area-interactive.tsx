"use client"

import * as React from "react"
import { Area, AreaChart, CartesianGrid, XAxis } from "recharts"

import { useIsMobile } from "@/hooks/use-mobile"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  ChartConfig,
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
} from "@/components/ui/chart"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import {
  ToggleGroup,
  ToggleGroupItem,
} from "@/components/ui/toggle-group"

export const description = "An interactive area chart"

const chartData = [
  { "date": "2025-04-19", "articles": 3, "system": 4, "blog": 6 },
  { "date": "2025-04-20", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-04-21", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-04-22", "articles": 4, "system": 1, "blog": 1 },
  { "date": "2025-04-23", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-04-24", "articles": 1, "system": 0, "blog": 1 },
  { "date": "2025-04-25", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-04-26", "articles": 4, "system": 1, "blog": 1 },
  { "date": "2025-04-27", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-04-28", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-04-29", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-04-30", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-01", "articles": 5, "system": 1, "blog": 2 },
  { "date": "2025-05-02", "articles": 4, "system": 0, "blog": 6 },
  { "date": "2025-05-03", "articles": 5, "system": 3, "blog": 2 },
  { "date": "2025-05-04", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-05", "articles": 5, "system": 1, "blog": 2 },
  { "date": "2025-05-06", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-07", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-05-08", "articles": 9, "system": 0, "blog": 1 },
  { "date": "2025-05-09", "articles": 5, "system": 1, "blog": 2 },
  { "date": "2025-05-10", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-11", "articles": 8, "system": 0, "blog": 7 },
  { "date": "2025-05-12", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-13", "articles": 5, "system": 1, "blog": 2 },
  { "date": "2025-05-14", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-15", "articles": 5, "system": 0, "blog": 2 },
  { "date": "2025-05-16", "articles": 4, "system": 0, "blog": 1 },
  { "date": "2025-05-17", "articles": 5, "system": 1, "blog": 2 },
  { "date": "2025-05-18", "articles": 2, "system": 0, "blog": 0 }
];

const chartConfig = {
  documents: {
    label: "Documents",
  },
  articles: {
    label: "articles",
    color: "var(--primary)",
  },
   blog: {
    label: "blog",
    color: "var(--primary)",
  },
  system: {
    label: "system",
    color: "var(--primary)",
  },
} satisfies ChartConfig

export function ChartAreaInteractive() {
  const isMobile = useIsMobile()
  const [timeRange, setTimeRange] = React.useState("30d")

  React.useEffect(() => {
    if (isMobile) {
      setTimeRange("7d")
    }
  }, [isMobile])

  const filteredData = chartData.filter((item) => {
    const date = new Date(item.date)
    const referenceDate = new Date("2025-05-18")
    let daysToSubtract = 30
    if (timeRange === "30d") {
      daysToSubtract = 30
    } else if (timeRange === "7d") {
      daysToSubtract = 7
    }
    else if (timeRange === "1d") {
      daysToSubtract = 1
    }
    const startDate = new Date(referenceDate)
    startDate.setDate(startDate.getDate() - daysToSubtract)
    return date >= startDate
  })

  return (
    <Card className="@container/card">
      <CardHeader>
        <CardTitle>Documents</CardTitle>
        <CardDescription>
          <span className="hidden @[540px]/card:block">
            Documents indexed per day by tessellation
          </span>
          <span className="@[540px]/card:hidden">Last 30 days</span>
        </CardDescription>
        <CardAction>
          <ToggleGroup
            type="single"
            value={timeRange}
            onValueChange={setTimeRange}
            variant="outline"
            className="hidden *:data-[slot=toggle-group-item]:!px-4 @[767px]/card:flex"
          >
            <ToggleGroupItem value="30d">Last 30 days</ToggleGroupItem>
            <ToggleGroupItem value="7d">Last 7 days</ToggleGroupItem>
            <ToggleGroupItem value="1d">Last 24 hours</ToggleGroupItem>
          </ToggleGroup>
          <Select value={timeRange} onValueChange={setTimeRange}>
            <SelectTrigger
              className="flex w-40 **:data-[slot=select-value]:block **:data-[slot=select-value]:truncate @[767px]/card:hidden"
              size="sm"
              aria-label="Select a value"
            >
              <SelectValue placeholder="Last 3 months" />
            </SelectTrigger>
            <SelectContent className="rounded-xl">
              <SelectItem value="30d" className="rounded-lg">
                Last 30 days
              </SelectItem>
              <SelectItem value="7d" className="rounded-lg">
                Last 7 days
              </SelectItem>
              <SelectItem value="1d" className="rounded-lg">
                Last 24 hours
              </SelectItem>
            </SelectContent>
          </Select>
        </CardAction>
      </CardHeader>
      <CardContent className="px-2 pt-4 sm:px-6 sm:pt-6">
        <ChartContainer
          config={chartConfig}
          className="aspect-auto h-[250px] w-full"
        >
          <AreaChart data={filteredData}>
            <defs>
              <linearGradient id="fillArticles" x1="0" y1="0" x2="0" y2="1">
                <stop
                  offset="5%"
                  stopColor="#0044cd"
                  stopOpacity={1.0}
                />
                <stop
                  offset="95%"
                  stopColor="#0044cd"
                  stopOpacity={0.1}
                />
              </linearGradient>
               <linearGradient id="fillBlog" x1="0" y1="0" x2="0" y2="1">
                <stop
                  offset="5%"
                  stopColor="#20a716"
                  stopOpacity={1.0}
                />
                <stop
                  offset="95%"
                  stopColor="#20a716"
                  stopOpacity={0.1}
                />
              </linearGradient>
              <linearGradient id="fillSystem" x1="0" y1="0" x2="0" y2="1">
                <stop
                  offset="5%"
                  stopColor="#ff6600"
                  stopOpacity={0.8}
                />
                <stop
                  offset="95%"
                  stopColor="#ff6600"
                  stopOpacity={0.1}
                />
              </linearGradient>
            </defs>
            <CartesianGrid vertical={false} />
            <XAxis
              dataKey="date"
              tickLine={false}
              axisLine={false}
              tickMargin={8}
              minTickGap={32}
              tickFormatter={(value) => {
                const date = new Date(value)
                return date.toLocaleDateString("en-US", {
                  month: "short",
                  day: "numeric",
                })
              }}
            />
            <ChartTooltip
              cursor={false}
              defaultIndex={isMobile ? -1 : 10}
              content={
                <ChartTooltipContent
                  className="p-4 opacity-80"
                  labelClassName="pb-3"
                  labelFormatter={(value) => {
                    return new Date(value).toLocaleDateString("en-US", {
                      month: "short",
                      day: "numeric",
                    })
                  }}
                  indicator="dot"
                />
              }
            />
            <Area
              dataKey="system"
              type="natural"
              fill="url(#fillSystem)"
              stroke="#ff6600"
              stackId="a"
            />
            <Area
              dataKey="articles"
              type="natural"
              fill="url(#fillArticles)"
              stroke="#0044cd"
              stackId="a"
            />
            <Area
              dataKey="blog"
              type="natural"
              fill="url(#fillBlog)"
              stroke="#20a716"
              stackId="a"
            />
          </AreaChart>
        </ChartContainer>
      </CardContent>
    </Card>
  )
}
