# Copyright The OpenTelemetry Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.


from collections.abc import Mapping, Sequence
from typing import TypeAlias

AttributeValue = str | bool | int | float | Sequence[str] | Sequence[bool] | Sequence[int] | Sequence[float]
Attributes = Mapping[str, AttributeValue] | None
AttributesAsKey = tuple[
    tuple[
        str,
        str | bool | int | float | tuple[str | None, ...] | tuple[bool | None, ...] | tuple[int | None, ...] | tuple[float | None, ...],
    ],
    ...,
]
TOOL_NAME: TypeAlias = str
SERVER_NAME: TypeAlias = str
