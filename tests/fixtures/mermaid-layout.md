# Mermaid layout cases

## Long Chinese labels with explicit ELK layout

```mermaid
---
config:
  layout: elk
---
flowchart TD
    A["图书馆收到读者提交的申请"] -->|"借阅申请：检查读者账户和馆藏状态"| B{"馆内有可借阅的副本，且读者的借阅数量未达到上限？"}
    A -->|"预约申请：加入等候名单"| D
    B -->|"是"| C["办理借阅手续<br/>登记归还日期并向读者发送确认消息"]
    B -->|"否"| D["登记预约申请<br/>有可用副本时按预约顺序通知读者"]
```

## Explicit line breaks and shorter labels

```mermaid
flowchart TD
    A["读者申请"] -->|"借阅"| B{"馆内有副本<br/>且未达到借阅上限？"}
    A -->|"预约"| D
    B -->|"是"| C["办理借阅<br/>登记归还日期"]
    B -->|"否"| D["登记预约<br/>等待可用副本"]
```

## Wide horizontal chain

```mermaid
flowchart LR
    A[Receive book] --> B[Check condition]
    B --> C[Check catalog]
    C --> D[Assign shelf]
    D --> E[Print label]
    E --> F[Shelve book]
    F --> G[Update catalog]
```

## Nested groups, shared targets and feedback

```mermaid
flowchart TD
    subgraph Intake
        A[Receive return] --> B{Book condition}
        B -->|"Ready for another loan"| C[Reshelve]
        B -->|"Needs repair before it can be borrowed"| D[Send to repair]
    end
    subgraph Catalog
        C --> E[Update record]
        D --> E
        E --> F[Check reservations]
    end
    F -. "Repaired books return to intake" .-> A
```

## State diagram with parallel transitions

```mermaid
stateDiagram-v2
    [*] --> Available
    Available --> OnLoan: Reader checks out the book
    Available --> Reserved: Reader places a hold
    OnLoan --> Available: Reader returns the book
    Reserved --> Available: Hold expires
    Reserved --> Available: Reader cancels the hold
```

## Sequence diagram as a control

```mermaid
sequenceDiagram
    participant Reader
    participant Library
    Reader->>Library: Request book
    Library-->>Reader: Confirm reservation
```

## Long Chinese labels with the default layout

This example repeats the first graph with the default layout.

```mermaid
flowchart TD
    A["图书馆收到读者提交的申请"] -->|"借阅申请：检查读者账户和馆藏状态"| B{"馆内有可借阅的副本，且读者的借阅数量未达到上限？"}
    A -->|"预约申请：加入等候名单"| D
    B -->|"是"| C["办理借阅手续<br/>登记归还日期并向读者发送确认消息"]
    B -->|"否"| D["登记预约申请<br/>有可用副本时按预约顺序通知读者"]
```
